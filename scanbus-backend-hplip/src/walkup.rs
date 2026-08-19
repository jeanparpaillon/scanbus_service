//! Appearing under *Scan → Computer* on an HP panel.
//!
//! An HP network MFP offers a host on its panel only once that host has been written
//! into `/WalkupScanToComp/WalkupScanToCompDestinations`, a LEDM collection the device
//! serves over plain HTTP. Nothing in HPLIP performs that write — `grep -rl walkup
//! /usr/share/hplip` matches nothing in 3.24.4 — so [`crate`]'s listener, which parses
//! `hpssd`'s `scanWaitingForPC` event, is waiting for an event the device does not emit
//! until this module has spoken to it.
//!
//! # The lifetime model is the opposite of Brother's
//!
//! The Brother backend's `registrar` leans on `DURATION=360`: the panel entry is a
//! lease, it lapses by itself, and a daemon that crashes leaves nothing behind. HP has
//! no lease field. A destination is a REST resource that persists until it is
//! `DELETE`d — across crashes, suspends, network changes and reboots — and
//! `WalkupScanToCompCaps` caps the collection at 15 (`MaxNetworkDestinations`). So a
//! leaked entry is both the worst failure available (the host is on the panel, the user
//! selects it, nothing happens, permanently) and a bounded resource a crash loop
//! exhausts. Registration therefore sweeps this host's own stale entries before adding
//! one, and deregistration actually sends something.
//!
//! # Why a transport trait
//!
//! Same reason the Brother backend's `SnmpTransport` exists: the interesting cases are
//! the device's answers, and none of them can be staged on hardware in a test suite. The
//! device distinguishes a transport mistake from a document mistake with exactly one
//! status — `415` for a `Content-Type` of `application/xml` versus a bare `400` with an
//! empty entity for a body it dislikes — and getting that classification right is worth
//! a test rather than a printer.
//!
//! The wire format is small enough to hand-roll on [`tokio::net::TcpStream`] rather than
//! to justify `reqwest`: three request shapes, no TLS, no redirects, no auth.
//! [`Request`] and [`Response`] are therefore as much HTTP as this backend has, and no
//! more; [`TcpHttp`] is the one implementation that opens a socket.

use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use quick_xml::Reader;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesRef, Event};
use scanbus_core::{BackendError, ScannerId};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info};

/// Where a device serves LEDM. Plain HTTP: the walk-up resources are not offered over
/// TLS, and hpmud does not speak to them over one either.
pub const DEFAULT_PORT: u16 = 80;

/// The capability document. Its absence — a `404` — is how a USB-attached HP says it has
/// no walk-up registration, which is a skip and not a failure: `hpssd` reports the same
/// `scanWaitingForPC` off the USB status channel with no destination involved.
pub const CAPS_PATH: &str = "/WalkupScanToComp/WalkupScanToCompCaps";

/// The collection of registered hosts: `GET` to list, `POST` to add. Both spellings are
/// in the device's own `WalkupScanToCompManifest.xml`.
pub const DESTINATIONS_PATH: &str = "/WalkupScanToComp/WalkupScanToCompDestinations";

/// The only `Content-Type` the device accepts for a registration.
///
/// `application/xml` is answered `415`, which is the one response that distinguishes a
/// transport mistake from a body mistake — see [`Rejection::WrongMediaType`].
pub const XML_MEDIA_TYPE: &str = "text/xml";

/// The statuses this module reasons about, named where they are compared.
pub mod status {
    /// A collection or capability document came back.
    pub const OK: u16 = 200;
    /// A destination was registered; `Location` carries its URI.
    pub const CREATED: u16 = 201;
    /// A destination was deleted.
    pub const NO_CONTENT: u16 = 204;
    /// The document was refused. The device sends no entity with it.
    pub const BAD_REQUEST: u16 = 400;
    /// No such resource — on [`CAPS_PATH`](super::CAPS_PATH), the USB case.
    pub const NOT_FOUND: u16 = 404;
    /// The `Content-Type` was refused, whatever the document said.
    pub const UNSUPPORTED_MEDIA_TYPE: u16 = 415;
}

/// One device's HTTP endpoint.
///
/// A host and a port rather than a URL, because that is all there is: every target in
/// this module is a constant path or a `Location` the device itself handed back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    host: String,
    port: u16,
}

impl Device {
    /// A device at `host` on [`DEFAULT_PORT`].
    ///
    /// `host` is whatever the hpmud `device_uri` carried — an address, a `.local.` name,
    /// or a Bonjour name — and is not resolved here: resolution is the transport's, and
    /// a name that does not resolve is the same failure as a device that does not
    /// answer.
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: DEFAULT_PORT,
        }
    }

    /// A device on a port other than 80. Nothing in the field needs it; tests do.
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// The device an hpmud `device_uri` names, when it is one this module can reach.
    ///
    /// hpmud puts the address in the query string — `hp:/net/OfficeJet_250?ip=192.168.1.3`,
    /// or `?zc=HP1AB2C3.local.` for a device found by Bonjour — and
    /// `physical_address_from_uri` in [`crate`] already reads those same three keys. Read
    /// again here rather than taking the scanner id apart, because that id is lowercased
    /// and punctuation-escaped on its way to a D-Bus object path, and what a socket is
    /// opened to has to stay exactly the string hpmud published.
    ///
    /// The order is `ip`, then `hostname`, then `zc`, which is the order in [`crate`] and
    /// for the same reason: a literal address needs no resolver, and the resolver is where
    /// this fails on a laptop whose mDNS stack is asleep.
    ///
    /// `None` for a USB device. `hp:/usb/OfficeJet_250?serial=CN12345` carries no host
    /// because there is nothing to connect to, and such a device serves no
    /// `/WalkupScanToComp` either — that is the skip this issue's `404` case describes,
    /// reached one step earlier.
    pub fn from_device_uri(device_uri: &str) -> Option<Self> {
        let query = device_uri.split_once('?')?.1;
        let mut ip = None;
        let mut hostname = None;
        let mut zc = None;

        for pair in query.split('&') {
            // A pair with no `=` is not one of ours; hpmud emits `&queue=false` and
            // friends, and a future key that is a bare flag must not abort the parse.
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            match key {
                "ip" => ip = Some(value),
                "hostname" => hostname = Some(value),
                "zc" => zc = Some(value),
                _ => {}
            }
        }

        // The trailing dot of a fully qualified mDNS name goes, as it does in
        // `physical_address_from_uri`: `HP1AB.local.` and `HP1AB.local` resolve to the
        // same device, and the undotted form is what every other client puts in `Host:`.
        let host = ip.or(hostname).or(zc)?.trim_end_matches('.');
        (!host.is_empty()).then(|| Self::new(host))
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// What goes in the `Host:` header, and what a connect is addressed to.
    ///
    /// The default port is left off — `Host: 192.168.1.3`, as every other client on the
    /// network sends it — and a literal IPv6 address is bracketed, because `host:port`
    /// is otherwise unparseable. Neither case is hypothetical for the bracketing's sake:
    /// hpmud publishes IPv4 and names, but the same string reaches
    /// [`std::net::ToSocketAddrs`] and must be well-formed there.
    pub fn authority(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == DEFAULT_PORT {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.authority())
    }
}

/// The three request shapes the walk-up resources need, and no others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Delete,
}

impl Method {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Delete => "DELETE",
        }
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One request to a device.
///
/// `target` is an origin-form path — a constant from this module, or the `Location` the
/// device returned for a destination it created. It is never built from user input, so
/// there is nothing here to escape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    method: Method,
    target: String,
    /// `None` on a request with no entity, so that the transport can leave the header
    /// out rather than send an empty one — `Content-Type` on a `GET` is meaningless and
    /// this device is fussy enough about the header to be worth not testing.
    content_type: Option<&'static str>,
    body: Vec<u8>,
}

impl Request {
    pub fn get(target: impl Into<String>) -> Self {
        Self::without_body(Method::Get, target)
    }

    pub fn delete(target: impl Into<String>) -> Self {
        Self::without_body(Method::Delete, target)
    }

    /// A `POST` of `body` as [`XML_MEDIA_TYPE`] — the only media type that gets past the
    /// device, so it is not a parameter.
    pub fn post_xml(target: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self {
            method: Method::Post,
            target: target.into(),
            content_type: Some(XML_MEDIA_TYPE),
            body: body.into(),
        }
    }

    /// A `POST` of `body` under some other media type. Only a test wants this: it is how
    /// the `415` path is reached deliberately rather than by accident.
    pub fn post_as(
        target: impl Into<String>,
        content_type: &'static str,
        body: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            method: Method::Post,
            target: target.into(),
            content_type: Some(content_type),
            body: body.into(),
        }
    }

    fn without_body(method: Method, target: impl Into<String>) -> Self {
        Self {
            method,
            target: target.into(),
            content_type: None,
            body: Vec::new(),
        }
    }

    pub fn method(&self) -> Method {
        self.method
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn content_type(&self) -> Option<&'static str> {
        self.content_type
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

impl fmt::Display for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.method, self.target)
    }
}

/// What came back: a status, the headers, and whatever entity followed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    status: u16,
    /// Kept in arrival order rather than in a map. There are three or four of them, one
    /// of which is read, and a map would only add a lowercasing policy to a type that
    /// otherwise just carries bytes.
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, headers: Vec<(String, String)>, body: Vec<u8>) -> Self {
        Self {
            status,
            headers,
            body,
        }
    }

    /// A response with no headers — most of what the device sends that is not a `201`.
    pub fn status_only(status: u16) -> Self {
        Self::new(status, Vec::new(), Vec::new())
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Header lookup, case-insensitively: header names are not case-sensitive and this
    /// device does not spell them the way any particular reader expects.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The URI of a resource the device just created — what a later `DELETE` addresses.
    pub fn location(&self) -> Option<&str> {
        self.header("Location")
    }

    pub const fn is_success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }
}

/// What carries one HTTP exchange to a device.
///
/// A trait so the classification above it can be driven with the device's own recorded
/// answers — `201` with a `Location`, a bare `400`, a `415` — none of which a test can
/// obtain from a printer. [`TcpHttp`] is the only implementation that opens a socket.
#[async_trait]
pub trait WalkupTransport: fmt::Debug + Send + Sync + 'static {
    /// Send `request` to `device` and return its answer, whatever the status.
    ///
    /// A `4xx` is a [`Response`], not an error: which statuses are failures depends on
    /// what was asked, and `404` on [`CAPS_PATH`] is a supported outcome.
    async fn exchange(
        &self,
        device: &Device,
        request: &Request,
    ) -> Result<Response, TransportError>;
}

/// Everything that can go wrong before a status comes back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// Nothing arrived in time.
    Timeout(Duration),
    /// The socket failed: no route, no listener, a name that does not resolve, a
    /// connection closed mid-response.
    Io(String),
    /// Something answered and it was not an HTTP response this module can read.
    Malformed(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout(after) => write!(f, "no answer within {after:?}"),
            Self::Io(error) => write!(f, "connection failed: {error}"),
            Self::Malformed(reason) => write!(f, "unreadable answer: {reason}"),
        }
    }
}

impl std::error::Error for TransportError {}

/// Why a device said no, in the terms the device itself distinguishes.
///
/// The split is not cosmetic: `400` and `415` are the only signal available about *which
/// half* of a registration is wrong, since the device sends no entity with either. A
/// `415` means the request never reached the document — the `Content-Type` was refused —
/// and a `400` means the document was read and disliked. Collapsing them into one
/// "rejected" would throw away the only diagnostic the protocol offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// `400`: the document was refused. Eighteen spellings taken from the device's own
    /// `GET` responses earn this, which is why the registration body is a recorded
    /// constant rather than something rebuilt from the schema.
    BadDocument,
    /// `415`: the media type was refused, so nothing was said about the document.
    WrongMediaType,
    /// `404`: no such resource.
    NotFound,
    /// Any other non-success status.
    Status(u16),
}

impl Rejection {
    /// Classify a status. Only called for statuses the caller did not expect.
    pub const fn of(status: u16) -> Self {
        match status {
            status::BAD_REQUEST => Self::BadDocument,
            status::UNSUPPORTED_MEDIA_TYPE => Self::WrongMediaType,
            status::NOT_FOUND => Self::NotFound,
            other => Self::Status(other),
        }
    }

    /// The status that produced this rejection.
    pub const fn status(self) -> u16 {
        match self {
            Self::BadDocument => status::BAD_REQUEST,
            Self::WrongMediaType => status::UNSUPPORTED_MEDIA_TYPE,
            Self::NotFound => status::NOT_FOUND,
            Self::Status(status) => status,
        }
    }
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadDocument => write!(f, "400: the device refused the document"),
            Self::WrongMediaType => write!(f, "415: the device refused the Content-Type"),
            Self::NotFound => write!(f, "404: no such resource on the device"),
            Self::Status(status) => write!(f, "{status}"),
        }
    }
}

/// Why a walk-up exchange did not produce what was asked for.
///
/// Two cases, kept apart for the same reason Brother's registrar keeps them apart: a
/// device that does not answer is off, asleep or on another network and is worth a
/// retry, while a device that answers and refuses is saying something about this
/// request that retrying will not change.
///
/// [`Full`](Self::Full) is a third, and is the one this protocol adds: nothing is wrong
/// with the device or with the request, the collection is simply full, and the only thing
/// that clears it is a person at the printer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkupError {
    /// The device did not answer at all.
    Unreachable {
        device: Device,
        request: String,
        source: TransportError,
    },
    /// The device answered, with a status that was not one of the expected ones.
    Refused {
        device: Device,
        request: String,
        rejection: Rejection,
    },
    /// The device answered with a document this module could not read.
    ///
    /// Distinct from [`TransportError::Malformed`], which is about the HTTP framing: this
    /// one is a well-formed `200` whose entity is not the collection it should be.
    Malformed {
        device: Device,
        request: String,
        reason: String,
    },
    /// The collection already holds `limit` destinations, none of them this host's.
    ///
    /// Counted before the `POST` rather than read off the device's answer to one, because
    /// every refusal this device has been observed to give is a bare status with an empty
    /// entity: a full collection cannot say so. Whatever status it picks would arrive as
    /// [`Rejection`], and a `400` there reads as *the printer refused the registration
    /// document* — sending whoever reads the bug report after the XML, when the actual
    /// problem is fifteen other machines and the fix is at the panel.
    Full { device: Device, limit: u32 },
}

impl WalkupError {
    /// True when the device answered and said no — as opposed to not answering.
    ///
    /// [`Full`](Self::Full) is not one: the device never refused anything, it was never
    /// asked. Nothing was sent, so there is nothing on the device to undo.
    pub const fn is_refusal(&self) -> bool {
        matches!(self, Self::Refused { .. })
    }

    /// The scanbus error to report for this failure.
    ///
    /// Silence is [`BackendError::NotReachable`], i.e. `org.scanbus.Error.NotReachable`:
    /// switch the printer on. A refusal is not — there is nothing for the user to
    /// power-cycle — so it lands on [`BackendError::Other`] with the status spelled out,
    /// which is what makes a `415` from a future firmware readable in a bug report
    /// rather than indistinguishable from an unplugged printer. A document that could not
    /// be read goes the same way and for the same reason: the device is there, it is
    /// answering, and what it said needs to reach whoever reads the log.
    ///
    /// [`Full`](Self::Full) lands there too, and its message is the only one here that
    /// names something the user can do: the collection has no eviction policy and no
    /// lease, so the fifteenth entry is cleared at the printer's panel or not at all.
    pub fn into_backend_error(self, scanner: &ScannerId) -> BackendError {
        match self {
            Self::Unreachable { .. } => BackendError::NotReachable {
                scanner: scanner.clone(),
                detail: self.to_string(),
            },
            Self::Refused { .. } | Self::Malformed { .. } | Self::Full { .. } => {
                BackendError::Other(format!("scanner {scanner}: {self}"))
            }
        }
    }
}

impl fmt::Display for WalkupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable {
                device,
                request,
                source,
            } => write!(f, "{device} did not answer {request}: {source}"),
            Self::Refused {
                device,
                request,
                rejection,
            } => write!(f, "{device} answered {request} with {rejection}"),
            Self::Malformed {
                device,
                request,
                reason,
            } => write!(f, "{device} answered {request} unreadably: {reason}"),
            Self::Full { device, limit } => write!(
                f,
                "{device} already holds the {limit} Scan to Computer destinations it \
                 allows, and none of them is this host; remove one at the printer, under \
                 Scan to Computer, and pair again",
            ),
        }
    }
}

impl std::error::Error for WalkupError {}

/// The walk-up resources of one device.
///
/// One per device, like Brother's `Registrar` and for the same reason: the collection,
/// its 15-entry limit and its failures all belong to a single printer, and a printer
/// that is unplugged must not hold up another one.
#[derive(Debug, Clone)]
pub struct Walkup {
    device: Device,
    transport: Arc<dyn WalkupTransport>,
}

impl Walkup {
    pub fn new(device: Device, transport: Arc<dyn WalkupTransport>) -> Self {
        Self { device, transport }
    }

    pub fn device(&self) -> &Device {
        &self.device
    }

    /// Send one request and return whatever came back, including a `4xx`.
    ///
    /// The caller classifies: `404` from [`CAPS_PATH`] is a device without walk-up
    /// registration and not an error, so this cannot decide on its own that a
    /// non-success status has failed.
    pub async fn exchange(&self, request: &Request) -> Result<Response, WalkupError> {
        let response = self
            .transport
            .exchange(&self.device, request)
            .await
            .map_err(|source| WalkupError::Unreachable {
                device: self.device.clone(),
                request: request.to_string(),
                source,
            })?;

        debug!(
            device = %self.device,
            request = %request,
            status = response.status(),
            "walk-up exchange",
        );
        Ok(response)
    }

    /// Send one request and insist on one of `expected`.
    ///
    /// `expected` is a list rather than a single status because the device is not
    /// consistent about which success it uses — a `DELETE` may answer `200` or `204` —
    /// and because treating every `2xx` as success would swallow a `202 Accepted`, which
    /// would mean the destination is not on the panel yet and the caller has been told
    /// it is.
    pub async fn expect(
        &self,
        request: &Request,
        expected: &[u16],
    ) -> Result<Response, WalkupError> {
        let response = self.exchange(request).await?;
        if expected.contains(&response.status()) {
            Ok(response)
        } else {
            Err(WalkupError::Refused {
                device: self.device.clone(),
                request: request.to_string(),
                rejection: Rejection::of(response.status()),
            })
        }
    }

    /// What `WalkupScanToCompCaps` says, or `None` when the device does not serve it.
    ///
    /// `None` is the USB case and is not a failure. A USB-attached HP has no
    /// `/WalkupScanToComp` at all and needs none: `hpssd` reports the same
    /// `scanWaitingForPC` off the USB status channel, with no destination involved. So a
    /// `404` here means *this device has nothing to register on*, which the caller
    /// answers by listening anyway — and the only way to tell that apart from a device
    /// that is off is to have had an answer at all, which is why this is a `404` and not
    /// a timeout.
    ///
    /// Any other non-`200` is a refusal: a device that serves LEDM and answers `500` for
    /// its own capability document is not a USB printer, and treating it as one would
    /// register nothing and say nothing.
    pub async fn limits(&self) -> Result<Option<Limits>, WalkupError> {
        let request = Request::get(CAPS_PATH);
        let response = self
            .expect(&request, &[status::OK, status::NOT_FOUND])
            .await?;
        if response.status() == status::NOT_FOUND {
            return Ok(None);
        }

        parse_capabilities(response.body())
            .map(Some)
            .map_err(|error| WalkupError::Malformed {
                device: self.device.clone(),
                request: request.to_string(),
                reason: error.to_string(),
            })
    }

    /// Put this host on the panel, if this device has a panel to put it on.
    ///
    /// The whole flow in one call, in the one order that is safe: read the capabilities,
    /// then — only for a device that has them — sweep and register within them.
    /// `Ok(None)` is the USB case from [`limits`](Self::limits). Fetching the capability
    /// document is not an optional extra step for the sake of one number: it is the only
    /// thing that tells a device with no walk-up collection apart from one that has a
    /// collection and would not take this host, and those two want opposite answers —
    /// carry on listening, or fail the connect.
    pub async fn install(&self, name: &str) -> Result<Option<Registration>, WalkupError> {
        let Some(limits) = self.limits().await? else {
            return Ok(None);
        };
        self.register(name, &limits).await.map(Some)
    }

    /// Everything currently registered on this device, this host's entries included.
    ///
    /// The list the sweep works from, and the answer to "why is my machine not on the
    /// panel" — an empty collection on a device that pairs cleanly is exactly the symptom
    /// this module exists to fix.
    pub async fn destinations(&self) -> Result<Vec<Destination>, WalkupError> {
        let request = Request::get(DESTINATIONS_PATH);
        let response = self.expect(&request, &[status::OK]).await?;
        parse_destinations(response.body()).map_err(|error| WalkupError::Malformed {
            device: self.device.clone(),
            request: request.to_string(),
            reason: error.to_string(),
        })
    }

    /// Register this host under `name`, after removing whatever it left behind.
    ///
    /// The sweep comes first and is not optional. There is no lease here — a destination
    /// persists until something deletes it — so a daemon that crashed, was killed, lost
    /// the network or was upgraded has left its entry on the panel, and registering again
    /// without collecting it puts this host on the panel twice, once live and once dead.
    /// The user cannot tell them apart: both show [`panel_name`]. Selecting the dead one
    /// is the failure this module exists to prevent, and it does not clear itself.
    ///
    /// Deleting first also keeps this host's footprint at one entry of the fifteen
    /// `MaxNetworkDestinations` allows, which is what stops a crash loop from filling the
    /// collection and locking every other machine out of it.
    ///
    /// The `Location` of the new entry is the only handle on it, so a `201` without one
    /// is [`WalkupError::Malformed`] rather than a success: this host would be on the
    /// panel with nothing able to address it. It is not a leak that accumulates — the
    /// next start sweeps it, by name, like any other stale entry — but it is one this run
    /// cannot undo, and reporting a registration that cannot be reversed as `Ok` would
    /// leave `stop_listening` with nothing to send.
    ///
    /// `limits` comes from [`limits`](Self::limits) — normally through
    /// [`install`](Self::install), which is what a caller wants — and the room left is
    /// counted *after* the sweep, not before it. A host that crash-looped its way to
    /// fourteen entries of its own is looking at a full collection it is about to empty;
    /// reporting [`WalkupError::Full`] there would send the user to the panel to clear
    /// entries this call clears itself. Only the entries that are still there once ours
    /// are gone are somebody else's, and only those are a reason to stop.
    pub async fn register(&self, name: &str, limits: &Limits) -> Result<Registration, WalkupError> {
        let occupied = self.destinations().await?;
        let swept = self.remove_all(name, &occupied).await?;

        if let Some(limit) = limits.max_network_destinations()
            && occupied.len() - swept.len() >= limit as usize
        {
            return Err(WalkupError::Full {
                device: self.device.clone(),
                limit,
            });
        }

        let request = Request::post_xml(DESTINATIONS_PATH, destination_document(name));
        let response = self.expect(&request, &[status::CREATED]).await?;
        let uri = response
            .location()
            .ok_or_else(|| WalkupError::Malformed {
                device: self.device.clone(),
                request: request.to_string(),
                reason: "201 Created without a Location, so the entry it made has no URI to \
                         delete it by"
                    .to_owned(),
            })?
            .to_owned();

        info!(
            device = %self.device,
            name,
            uri = %uri,
            swept = swept.len(),
            "registered this host under Scan to Computer",
        );
        Ok(Registration { uri, swept })
    }

    /// The sweep: delete every entry of `occupied` that `name` registered, and return
    /// them.
    ///
    /// Matched on `dd3:Hostname`, which is why the collection is parsed rather than
    /// grepped: a `DELETE` addressed on a substring match is another machine's entry
    /// disappearing from the panel.
    ///
    /// A failed delete fails the sweep. Carrying on would register a second entry beside
    /// a stale one that could not be removed, which is precisely the state being swept
    /// for; a device that answers `500` to a `DELETE` is better reported than worked
    /// around.
    ///
    /// Takes the collection rather than fetching it, so that
    /// [`register`](Self::register) can count what is left in it from the same `GET` —
    /// one round trip, and one view of a collection that other hosts are also writing to.
    async fn remove_all(
        &self,
        name: &str,
        occupied: &[Destination],
    ) -> Result<Vec<Destination>, WalkupError> {
        let mut swept = Vec::new();
        for destination in occupied.iter().filter(|entry| entry.registered_by(name)) {
            self.remove(destination.uri()).await?;
            info!(
                device = %self.device,
                entry = %destination,
                "removed a walk-up destination this host had left behind",
            );
            swept.push(destination.clone());
        }
        Ok(swept)
    }

    /// Delete one destination by its URI — a [`Registration::uri`], or an entry the sweep
    /// found.
    ///
    /// `404` counts as success. The collection is shared: another client, another daemon
    /// on this host, or someone at the panel may have removed the entry between the `GET`
    /// that listed it and this `DELETE`, and the outcome asked for is that the entry is
    /// gone, which it is.
    pub async fn remove(&self, uri: &str) -> Result<(), WalkupError> {
        self.expect(
            &Request::delete(uri),
            &[status::OK, status::NO_CONTENT, status::NOT_FOUND],
        )
        .await
        .map(|_| ())
    }
}

/// What a successful [`Walkup::register`] did.
///
/// Both halves are worth keeping. The URI is what `stop_listening` deletes — the device
/// never volunteers it again in that form, since the collection has to be fetched and
/// searched to find it — and the entries the sweep removed are the record that this host
/// had leaked, which is otherwise invisible: the symptom is on the printer's panel and
/// nowhere in the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    uri: String,
    swept: Vec<Destination>,
}

impl Registration {
    /// The `Location` the device returned: the URI of this host's entry, and the only
    /// handle on it.
    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// The stale entries of this host that registering removed. Empty on a clean start.
    pub fn swept(&self) -> &[Destination] {
        &self.swept
    }
}

/// What `WalkupScanToCompCaps` says about the collection.
///
/// One field, because one is what the document offers that this module can act on. The
/// `UserActionTimeout` beside it — 60 s on the recorded device — is how long the panel
/// waits for the user after the host has been selected, which belongs to the scan the
/// event starts and not to the registration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    max_network_destinations: Option<u32>,
}

impl Limits {
    /// How many destinations the collection holds, when the device said.
    ///
    /// 15 on the recorded OfficeJet 250. `None` when the document carried no
    /// `MaxNetworkDestinations` — an unknown limit, which is **not** a full device: a
    /// firmware that stopped publishing the element would otherwise take every host off
    /// its own panel, and the device is perfectly able to refuse a `POST` for itself.
    pub const fn max_network_destinations(&self) -> Option<u32> {
        self.max_network_destinations
    }

    /// The limits of a device that published one, for tests and for callers holding a
    /// number from elsewhere.
    pub const fn holding(max_network_destinations: u32) -> Self {
        Self {
            max_network_destinations: Some(max_network_destinations),
        }
    }
}

/// Where the machine's name is, on a running Linux system.
///
/// `/proc` first because it is the live value: `/etc/hostname` is what the machine was
/// called at boot, and a `hostnamectl set-hostname` since then has not changed it.
const HOSTNAME_FILES: [&str; 2] = ["/proc/sys/kernel/hostname", "/etc/hostname"];

/// What this host is called when the hostname yields nothing usable — an unreadable
/// `/proc`, or a name that is empty once trimmed. Recognisable on a panel, and true.
const FALLBACK_PANEL_NAME: &str = "scanbus";

/// The name this host appears under on the printer's panel.
///
/// It goes into the document twice, as `Hostname` and as `Name`, which is what HP's own
/// software does; the panel shows `Name`, and the sweep matches on `Hostname`.
///
/// Unlike Brother's `panel_name` there is no filter and no truncation. `USER=` is a
/// fifteen-character alphanumeric field in an SNMP string that the device parses by
/// position; these are dictionary elements in an XML document, the device took `spectre`
/// unchanged, and the only limit `WalkupScanToCompCaps` documents is
/// `MaxNetworkDestinations`. Inventing a restriction the device does not impose would put
/// a name on the panel that the user does not recognise, which is the one thing this
/// value has to avoid.
///
/// The domain is dropped all the same: `desktop.office.example.com` has to read as
/// `desktop` on a two-line LCD, and — since `Hostname` is what the sweep matches its own
/// stale entries on — the value must not change because a DHCP lease handed out a
/// different search domain.
///
/// Computed and **logged once**, on the first call. Not decoration: it is the only way to
/// find out what to look for on the panel without walking to the printer.
pub fn panel_name() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        let hostname = read_hostname();
        let name = panel_name_from(hostname.as_deref().unwrap_or(""));
        info!(
            panel_name = %name,
            hostname = hostname.as_deref().unwrap_or("<unreadable>"),
            "this host will appear under Scan to Computer on HP panels under this name",
        );
        name
    })
    .as_str()
}

/// [`panel_name`]'s rule, without the filesystem — every case in it is a real hostname.
fn panel_name_from(hostname: &str) -> String {
    let short = hostname.split('.').next().unwrap_or_default().trim();
    if short.is_empty() {
        FALLBACK_PANEL_NAME.to_owned()
    } else {
        short.to_owned()
    }
}

fn read_hostname() -> Option<String> {
    HOSTNAME_FILES.iter().find_map(|path| {
        let name = std::fs::read_to_string(path).ok()?;
        let name = name.trim().to_owned();
        (!name.is_empty()).then_some(name)
    })
}

/// The registration document for `name`, byte for byte as the device accepted it.
///
/// Recorded, not reconstructed. Eighteen spellings taken from the device's *own* GET
/// responses were rejected with a bare `400` and an empty entity, and all three traps are
/// invisible from the schema:
///
/// - the root element is `WalkupScanToComp`, **not** the `WalkupScanToCompDestination`
///   that the collection's own GET uses for the same object;
/// - `Hostname` is in dictionaries `2009/04/06`, while `Name` immediately beside it is in
///   `1.0`;
/// - the order is `Hostname`, `Name`, `LinkType`.
///
/// So this is a template with one value substituted and nothing else: no pretty printing,
/// no whitespace between elements, no reordering, no `WalkupScanToCompDestination`. It
/// goes out as [`XML_MEDIA_TYPE`]; `application/xml` is answered `415`, which is the only
/// response that tells a transport mistake apart from a body mistake.
///
/// `name` is escaped even though a hostname cannot legally contain `&` or `<`: the value
/// is read off the filesystem at startup, it is one `format!` away from the wire, and an
/// unescaped character would come back as the same bare `400` as every other body
/// mistake — with nothing in the response to say which.
pub fn destination_document(name: &str) -> Vec<u8> {
    let name = escape_text(name);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <WalkupScanToComp \
         xmlns=\"http://www.hp.com/schemas/imaging/con/ledm/walkupscan/2010/09/28\" \
         xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" \
         xsi:schemaLocation=\"http://www.hp.com/schemas/imaging/con/ledm/walkupscan/2010/09/28 \
         WalkupScan.xsd\">\
         <Hostname \
         xmlns=\"http://www.hp.com/schemas/imaging/con/dictionaries/2009/04/06\">\
         {name}</Hostname>\
         <Name xmlns=\"http://www.hp.com/schemas/imaging/con/dictionaries/1.0/\">\
         {name}</Name>\
         <LinkType>Network</LinkType>\
         </WalkupScanToComp>"
    )
    .into_bytes()
}

/// XML escaping for element content: the three characters that can end an element early.
fn escape_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// One entry of a device's destination collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    uri: String,
    hostname: String,
}

impl Destination {
    /// The resource path the device gave this entry, as `dd:ResourceURI` — what a
    /// `DELETE` is addressed to, and the only handle there is on it.
    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// `dd3:Hostname`: the host that registered the entry. Empty if it published none,
    /// which makes it an entry the sweep leaves alone rather than one it can claim.
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// Whether this is an entry `name` registered — what the sweep deletes on.
    ///
    /// Case-insensitively, because a host does not stop being itself for having been
    /// spelled `Spectre` on the run that registered it: hostnames are matched that way
    /// everywhere else, and an entry the sweep failed to recognise is a permanent dead
    /// entry on the panel.
    ///
    /// An entry with no `dd3:Hostname` is never ours, however empty `name` may be. It
    /// belongs to something that registered without publishing one, and deleting a
    /// destination on that guess is the mistake `quick-xml` was added here to avoid.
    pub fn registered_by(&self, name: &str) -> bool {
        !self.hostname.is_empty() && self.hostname.eq_ignore_ascii_case(name)
    }
}

impl fmt::Display for Destination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at {}", self.hostname, self.uri)
    }
}

/// A document the device sent that this module could not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedDocument(String);

impl MalformedDocument {
    pub fn reason(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MalformedDocument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MalformedDocument {}

/// The [`Limits`] of a `WalkupScanToCompCaps` document.
///
/// Only `MaxNetworkDestinations` is looked for, and its absence is not an error — see
/// [`Limits::max_network_destinations`]. A value that is present and not a number is,
/// though: that is a document this module is misreading, and guessing a limit from one is
/// how a device ends up reported full when it is empty.
///
/// Matched on the local name for the same reason [`parse_destinations`] is: the prefixes
/// are the document's own choice, not the schema's.
pub fn parse_capabilities(document: &[u8]) -> Result<Limits, MalformedDocument> {
    let mut reader = Reader::from_reader(document);
    let mut limits = Limits::default();
    let mut open = false;
    let mut value = String::new();
    let mut buffer = Vec::new();

    loop {
        buffer.clear();
        let event = reader
            .read_event_into(&mut buffer)
            .map_err(|error| MalformedDocument(error.to_string()))?;

        match event {
            Event::Start(element) => {
                open = element.name().local_name().as_ref() == b"MaxNetworkDestinations";
                value.clear();
            }
            Event::Text(text) if open => {
                let run = text
                    .xml10_content()
                    .map_err(|error| MalformedDocument(error.to_string()))?;
                value.push_str(&run);
            }
            Event::End(element) => {
                if element.name().local_name().as_ref() == b"MaxNetworkDestinations" {
                    let number = value.trim().parse().map_err(|_| {
                        MalformedDocument(format!(
                            "MaxNetworkDestinations is {:?}, which is not a count of \
                             destinations",
                            value.trim()
                        ))
                    })?;
                    limits.max_network_destinations = Some(number);
                }
                open = false;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(limits)
}

/// The entries of a `WalkupScanToCompDestinations` document, each paired with the host
/// that registered it.
///
/// A real XML reader rather than a substring match, because the sweep **deletes** what
/// this returns: pairing a `ResourceURI` with the wrong `Hostname` takes another machine
/// off the panel, and that failure is silent at both ends — the other machine's user
/// finds out at the printer, days later.
///
/// Elements are matched on their local name. The prefixes are bindings the document
/// chooses, not part of the schema: this firmware writes `dd:ResourceURI` and
/// `dd3:Hostname`, and one that bound the same two namespaces to different prefixes, or
/// defaulted them, would be just as correct. Everything else is ignored, `dd:Name`
/// included — the panel label is not what an entry is identified by.
///
/// An entry with no `ResourceURI`, or a blank one, is dropped rather than returned: it is
/// an entry this module cannot address, and one that silently deleted
/// `/WalkupScanToComp/WalkupScanToCompDestinations` itself would be worse than one that
/// admitted it saw nothing.
pub fn parse_destinations(document: &[u8]) -> Result<Vec<Destination>, MalformedDocument> {
    /// The two elements worth collecting from inside one entry.
    enum Field {
        Uri,
        Hostname,
    }

    /// Appends one run of character data to whichever field is open.
    ///
    /// A run, not the value: quick-xml reports every `&…;` as its own event and splits the
    /// surrounding text around it, so a name that had to be escaped arrives in pieces.
    fn append(entry: &mut (Option<String>, Option<String>), field: &Field, run: &str) {
        let value = match field {
            Field::Uri => &mut entry.0,
            Field::Hostname => &mut entry.1,
        };
        value.get_or_insert_with(String::new).push_str(run);
    }

    /// The text one `&…;` stands for.
    ///
    /// Character references are resolved arithmetically, named ones only against XML's five
    /// predefined entities: there is no DTD here that could define others, and a reference
    /// this cannot resolve is a malformed document rather than a silent hole in a value the
    /// sweep is about to match a `DELETE` on.
    fn resolve(reference: &BytesRef<'_>) -> Result<String, MalformedDocument> {
        if let Some(character) = reference
            .resolve_char_ref()
            .map_err(|error| MalformedDocument(error.to_string()))?
        {
            return Ok(character.to_string());
        }

        let name = reference
            .decode()
            .map_err(|error| MalformedDocument(error.to_string()))?;
        resolve_predefined_entity(&name)
            .map(str::to_owned)
            .ok_or_else(|| MalformedDocument(format!("unknown entity &{name};")))
    }

    // Text is deliberately *not* trimmed by the reader: it would eat the spaces on either
    // side of an escaped character as well, since those are the ends of their own runs.
    // Whitespace outside an element of interest is ignored anyway, and what is inside one
    // is trimmed once, whole, below.
    let mut reader = Reader::from_reader(document);

    let mut destinations = Vec::new();
    let mut entry: Option<(Option<String>, Option<String>)> = None;
    let mut field: Option<Field> = None;
    let mut buffer = Vec::new();

    loop {
        buffer.clear();
        let event = reader
            .read_event_into(&mut buffer)
            .map_err(|error| MalformedDocument(error.to_string()))?;

        match event {
            Event::Start(element) => match element.name().local_name().as_ref() {
                // Exact, not a prefix test: the collection's own root element is
                // `WalkupScanToCompDestinations`, one `s` away from each of its members.
                b"WalkupScanToCompDestination" => entry = Some((None, None)),
                b"ResourceURI" if entry.is_some() => field = Some(Field::Uri),
                b"Hostname" if entry.is_some() => field = Some(Field::Hostname),
                _ => field = None,
            },
            Event::Text(text) => {
                if let (Some(entry), Some(field)) = (entry.as_mut(), field.as_ref()) {
                    let run = text
                        .xml10_content()
                        .map_err(|error| MalformedDocument(error.to_string()))?;
                    append(entry, field, &run);
                }
            }
            Event::GeneralRef(reference) => {
                if let (Some(entry), Some(field)) = (entry.as_mut(), field.as_ref()) {
                    let run = resolve(&reference)?;
                    append(entry, field, &run);
                }
            }
            Event::End(element) => {
                if element.name().local_name().as_ref() == b"WalkupScanToCompDestination" {
                    // `take` whatever the shape: an entry the device closed without a
                    // usable `ResourceURI` is finished, and finished means gone from here.
                    if let Some((uri, hostname)) = entry.take() {
                        let uri = uri.unwrap_or_default().trim().to_owned();
                        if !uri.is_empty() {
                            destinations.push(Destination {
                                uri,
                                hostname: hostname.unwrap_or_default().trim().to_owned(),
                            });
                        }
                    }
                }
                field = None;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(destinations)
}

/// How long one exchange gets: connect, request, and the whole of the response.
///
/// Ten seconds, where Brother's registrar allows two. That is a UDP round trip on the
/// LAN; this is a TCP connection to an embedded HTTP server that may be part-way through
/// printing a page, and the sweep makes several of them in a row.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// How much of a response this module will hold on to before giving up on it.
///
/// The collection is at most fifteen short entries and nothing here transfers an image. A
/// device answering `Content-Length: 4294967295` is one to disconnect from, not one to
/// allocate for.
const MAX_RESPONSE: usize = 256 * 1024;

/// The real transport: one HTTP/1.1 exchange per connection, hand-rolled.
///
/// Hand-rolled rather than `reqwest`, for the reason Brother's registrar hand-rolls SNMP.
/// Three request shapes are needed, all of them to a plain-HTTP embedded server on the
/// local network: no TLS, no redirects, no authentication, no cookies, no connection
/// pool, no compression, no proxy. A general HTTP client would put `hyper`, `rustls` and a
/// trust store in the daemon in order to send `POST /WalkupScanToComp/…` to port 80.
///
/// `Connection: close` goes on every request. The device frames the response by closing,
/// which removes the one case a hand-rolled reader would otherwise get wrong, and there is
/// no idle connection left to be holding across a suspend or a change of network. Nothing
/// here is latency-sensitive: registration happens once per connect.
#[derive(Debug, Clone, Copy)]
pub struct TcpHttp {
    timeout: Duration,
}

impl Default for TcpHttp {
    fn default() -> Self {
        Self::new(RESPONSE_TIMEOUT)
    }
}

impl TcpHttp {
    pub const fn new(timeout: Duration) -> Self {
        Self { timeout }
    }

    async fn round_trip(
        &self,
        device: &Device,
        request: &Request,
    ) -> Result<Response, TransportError> {
        // `(host, port)` and not `authority()`: the header form leaves port 80 out, which
        // is exactly the half `connect` needs, and it brackets IPv6, which `connect` does
        // not want.
        let mut stream = TcpStream::connect((device.host(), device.port()))
            .await
            .map_err(|error| TransportError::Io(error.to_string()))?;
        stream
            .write_all(&serialise(device, request))
            .await
            .map_err(|error| TransportError::Io(error.to_string()))?;

        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(response) = complete_response(&buffer)? {
                return Ok(response);
            }
            let read = stream
                .read(&mut chunk)
                .await
                .map_err(|error| TransportError::Io(error.to_string()))?;
            if read == 0 {
                return response_at_eof(&buffer);
            }
            if buffer.len() + read > MAX_RESPONSE {
                return Err(TransportError::Malformed(format!(
                    "more than {MAX_RESPONSE} bytes"
                )));
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
    }
}

#[async_trait]
impl WalkupTransport for TcpHttp {
    async fn exchange(
        &self,
        device: &Device,
        request: &Request,
    ) -> Result<Response, TransportError> {
        match timeout(self.timeout, self.round_trip(device, request)).await {
            Ok(result) => result,
            Err(_) => Err(TransportError::Timeout(self.timeout)),
        }
    }
}

/// The request on the wire.
///
/// Four headers, deliberately. `Host` because HTTP/1.1 requires it; `Content-Type`
/// because the device answers `415` without the right one; `Content-Length` because a
/// request carrying an entity needs a length and this server is not going to negotiate a
/// chunked upload; `User-Agent` because an embedded web server's log is the only place a
/// support case can find out which program put a destination on the printer.
fn serialise(device: &Device, request: &Request) -> Vec<u8> {
    let mut head = format!(
        "{} {} HTTP/1.1\r\n\
         Host: {}\r\n\
         User-Agent: scanbus/{}\r\n\
         Connection: close\r\n",
        request.method(),
        request.target(),
        device.authority(),
        env!("CARGO_PKG_VERSION"),
    );
    if let Some(content_type) = request.content_type() {
        head.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    if !request.body().is_empty() {
        head.push_str(&format!("Content-Length: {}\r\n", request.body().len()));
    }
    head.push_str("\r\n");

    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(request.body());
    bytes
}

/// How the entity ends.
enum Framing {
    /// `Content-Length` bytes of it.
    Length(usize),
    /// `Transfer-Encoding: chunked`, terminated by its own zero chunk.
    Chunked,
    /// Neither: the entity is whatever arrives before the device closes.
    UntilClose,
}

/// Which of the three framings this response uses, per RFC 9112 §6.3 as far as one
/// embedded server needs it.
///
/// The status is read first because a `204` carries no entity whatever its headers say —
/// and `204` is one of the two answers a `DELETE` gives here. Waiting for a close that a
/// device with keep-alive ideas of its own will not send is how the teardown of every
/// registration becomes a ten-second timeout.
///
/// `Transfer-Encoding` beats `Content-Length` when a response carries both, because the
/// chunk framing is the self-terminating one; a response carrying both is a contradiction
/// worth resolving the same way every proxy resolves it.
fn framing(status: u16, headers: &[(String, String)]) -> Result<Framing, TransportError> {
    if status == status::NO_CONTENT || status == 304 || (100..200).contains(&status) {
        return Ok(Framing::Length(0));
    }

    let value = |name: &str| {
        headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };

    if let Some(encoding) = value("Transfer-Encoding") {
        if encoding
            .split(',')
            .any(|token| token.trim().eq_ignore_ascii_case("chunked"))
        {
            return Ok(Framing::Chunked);
        }
        return Err(TransportError::Malformed(format!(
            "Transfer-Encoding: {encoding}"
        )));
    }

    match value("Content-Length") {
        Some(length) => length
            .trim()
            .parse::<usize>()
            .map(Framing::Length)
            .map_err(|_| TransportError::Malformed(format!("Content-Length: {length}"))),
        None => Ok(Framing::UntilClose),
    }
}

type Head = (u16, Vec<(String, String)>, usize);

/// Status line and headers, once `buffer` holds all of them.
fn head(buffer: &[u8]) -> Result<Option<Head>, TransportError> {
    let Some(end) = find(buffer, b"\r\n\r\n") else {
        return Ok(None);
    };
    let head = std::str::from_utf8(&buffer[..end])
        .map_err(|_| TransportError::Malformed("headers that are not text".to_owned()))?;

    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| TransportError::Malformed(format!("status line {status_line:?}")))?;

    let mut headers = Vec::new();
    for line in lines {
        // A line with no colon is one this module has no use for: everything it reads is
        // a well-formed header, or the response is unusable for a different reason.
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    Ok(Some((status, headers, end + 4)))
}

/// The response if `buffer` already holds all of it, `None` while it does not.
fn complete_response(buffer: &[u8]) -> Result<Option<Response>, TransportError> {
    let Some((status, headers, body_start)) = head(buffer)? else {
        return Ok(None);
    };
    let body = &buffer[body_start..];

    match framing(status, &headers)? {
        Framing::Length(length) if body.len() < length => Ok(None),
        Framing::Length(length) => Ok(Some(Response::new(
            status,
            headers,
            body[..length].to_vec(),
        ))),
        Framing::Chunked => Ok(dechunk(body)?.map(|body| Response::new(status, headers, body))),
        // Only the close says where this one ends, so there is nothing to decide yet.
        Framing::UntilClose => Ok(None),
    }
}

/// The response the device meant by closing the connection.
///
/// A close is the end of the entity for a response that declared no length of its own,
/// and a truncation for one that did. Telling those apart is the whole of this function.
fn response_at_eof(buffer: &[u8]) -> Result<Response, TransportError> {
    let Some((status, headers, body_start)) = head(buffer)? else {
        return Err(TransportError::Malformed(if buffer.is_empty() {
            "the connection closed with no answer at all".to_owned()
        } else {
            "the connection closed part-way through the headers".to_owned()
        }));
    };
    let body = &buffer[body_start..];

    match framing(status, &headers)? {
        Framing::Length(length) if body.len() < length => Err(TransportError::Malformed(format!(
            "{} bytes of a {length}-byte entity",
            body.len()
        ))),
        Framing::Length(length) => Ok(Response::new(status, headers, body[..length].to_vec())),
        Framing::Chunked => dechunk(body)?
            .map(|body| Response::new(status, headers, body))
            .ok_or_else(|| TransportError::Malformed("an unterminated chunked entity".to_owned())),
        Framing::UntilClose => Ok(Response::new(status, headers, body.to_vec())),
    }
}

/// A chunked entity, or `None` while its terminating zero chunk has not arrived.
///
/// Chunked at all because an embedded server that builds a collection element by element
/// has no length to declare. Chunk extensions are skipped and trailers are not read: the
/// zero chunk is the end of the entity, and nothing here looks at a trailer.
fn dechunk(body: &[u8]) -> Result<Option<Vec<u8>>, TransportError> {
    let mut rest = body;
    let mut entity = Vec::new();

    loop {
        let Some(line_end) = find(rest, b"\r\n") else {
            return Ok(None);
        };
        let size = std::str::from_utf8(&rest[..line_end])
            .ok()
            .and_then(|line| {
                let size = line.split(';').next().unwrap_or_default().trim();
                usize::from_str_radix(size, 16).ok()
            })
            .ok_or_else(|| {
                TransportError::Malformed("a chunk size that is not hexadecimal".to_owned())
            })?;
        rest = &rest[line_end + 2..];

        if size == 0 {
            return Ok(Some(entity));
        }
        // The chunk, and the CRLF the sender puts after it.
        if rest.len() < size + 2 {
            return Ok(None);
        }
        entity.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tokio::net::TcpListener;
    use tokio::time::sleep;

    use super::*;

    /// A device that answers from a script and remembers what it was asked.
    ///
    /// Answers are consumed in order and the last one repeats, so a test that only cares
    /// about one exchange writes one answer.
    #[derive(Debug)]
    struct FakeDevice {
        seen: Mutex<Vec<(Device, Request)>>,
        answers: Mutex<Vec<Result<Response, TransportError>>>,
    }

    impl FakeDevice {
        fn answering(
            answers: impl IntoIterator<Item = Result<Response, TransportError>>,
        ) -> Arc<Self> {
            Arc::new(Self {
                seen: Mutex::new(Vec::new()),
                answers: Mutex::new(answers.into_iter().collect()),
            })
        }

        fn seen(&self) -> Vec<(Device, Request)> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl WalkupTransport for FakeDevice {
        async fn exchange(
            &self,
            device: &Device,
            request: &Request,
        ) -> Result<Response, TransportError> {
            self.seen
                .lock()
                .unwrap()
                .push((device.clone(), request.clone()));

            let mut answers = self.answers.lock().unwrap();
            if answers.len() > 1 {
                answers.remove(0)
            } else {
                answers[0].clone()
            }
        }
    }

    fn walkup_for(device: Arc<FakeDevice>) -> Walkup {
        Walkup::new(Device::new("192.168.1.3"), device)
    }

    /// The recorded `201`, headers and all.
    fn created() -> Response {
        Response::new(
            status::CREATED,
            vec![
                ("Content-Length".into(), "0".into()),
                (
                    "LOCATION".into(),
                    "/WalkupScanToComp/WalkupScanToCompDestinations/\
                     1c8d4e40-dabb-1f08-aa30-644ed7fe04c4"
                        .into(),
                ),
            ],
            Vec::new(),
        )
    }

    /// What the recorded OfficeJet 250 publishes, for tests that are not about the caps
    /// document itself.
    fn recorded_caps() -> Limits {
        Limits::holding(15)
    }

    #[test]
    fn the_default_port_is_left_out_of_the_authority() {
        assert_eq!(Device::new("192.168.1.3").authority(), "192.168.1.3");
        assert_eq!(
            Device::new("printer.local").with_port(8080).authority(),
            "printer.local:8080"
        );
    }

    /// Not what hpmud publishes today, and the string still has to be addressable.
    #[test]
    fn a_literal_ipv6_address_is_bracketed() {
        assert_eq!(Device::new("fe80::1").authority(), "[fe80::1]");
        assert_eq!(
            Device::new("fe80::1").with_port(8080).authority(),
            "[fe80::1]:8080"
        );
    }

    #[tokio::test]
    async fn a_request_reaches_the_transport_addressed_to_this_device() {
        let device = FakeDevice::answering([Ok(Response::status_only(status::OK))]);
        let walkup = walkup_for(device.clone());

        walkup
            .exchange(&Request::get(DESTINATIONS_PATH))
            .await
            .expect("the fake device answered");

        let seen = device.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, Device::new("192.168.1.3"));
        assert_eq!(seen[0].1.method(), Method::Get);
        assert_eq!(seen[0].1.target(), DESTINATIONS_PATH);
        // No entity, so no header claiming one.
        assert_eq!(seen[0].1.content_type(), None);
        assert!(seen[0].1.body().is_empty());
    }

    /// The media type is not a parameter of a registration, because only one works.
    #[tokio::test]
    async fn a_registration_post_is_sent_as_text_xml() {
        let device = FakeDevice::answering([Ok(created())]);
        let walkup = walkup_for(device.clone());

        let request = Request::post_xml(DESTINATIONS_PATH, "<WalkupScanToComp/>");
        let response = walkup
            .expect(&request, &[status::CREATED])
            .await
            .expect("201 is the expected status");

        assert_eq!(device.seen()[0].1.content_type(), Some(XML_MEDIA_TYPE));
        assert_eq!(device.seen()[0].1.body(), b"<WalkupScanToComp/>");
        assert_eq!(
            response.location(),
            Some(
                "/WalkupScanToComp/WalkupScanToCompDestinations/\
                 1c8d4e40-dabb-1f08-aa30-644ed7fe04c4"
            ),
            "the header is spelled LOCATION here, and lookup is case-insensitive"
        );
    }

    /// The whole reason the status is classified rather than compared: these two say
    /// different things about the same `POST`, and the device sends no entity with
    /// either.
    #[tokio::test]
    async fn a_body_refusal_and_a_media_type_refusal_read_differently() {
        let bad_document = FakeDevice::answering([Ok(Response::status_only(status::BAD_REQUEST))]);
        let error = walkup_for(bad_document)
            .expect(
                &Request::post_xml(DESTINATIONS_PATH, "<WalkupScanToCompDestination/>"),
                &[status::CREATED],
            )
            .await
            .expect_err("400 is not 201");
        assert!(error.is_refusal());
        assert!(
            error.to_string().contains("refused the document"),
            "{error}"
        );

        let wrong_type =
            FakeDevice::answering([Ok(Response::status_only(status::UNSUPPORTED_MEDIA_TYPE))]);
        let error = walkup_for(wrong_type)
            .expect(
                &Request::post_as(DESTINATIONS_PATH, "application/xml", "<WalkupScanToComp/>"),
                &[status::CREATED],
            )
            .await
            .expect_err("415 is not 201");
        assert!(
            error.to_string().contains("refused the Content-Type"),
            "{error}"
        );
    }

    /// `404` on the capability document is the USB case, so it must come back as a
    /// response to be read rather than as an error the caller has to unwrap.
    #[tokio::test]
    async fn a_404_is_a_response_and_not_a_failure() {
        let device = FakeDevice::answering([Ok(Response::status_only(status::NOT_FOUND))]);

        let response = walkup_for(device)
            .exchange(&Request::get(CAPS_PATH))
            .await
            .expect("the device answered, and 404 is an answer");

        assert_eq!(response.status(), status::NOT_FOUND);
        assert!(!response.is_success());
    }

    /// A `DELETE` may come back either way, which is why the expectation is a list.
    #[tokio::test]
    async fn both_delete_successes_are_accepted() {
        for status in [status::OK, status::NO_CONTENT] {
            let device = FakeDevice::answering([Ok(Response::status_only(status))]);
            let request = Request::delete(format!("{DESTINATIONS_PATH}/1c8d4e40"));

            walkup_for(device)
                .expect(&request, &[status::OK, status::NO_CONTENT])
                .await
                .unwrap_or_else(|error| panic!("{status} should be a deletion: {error}"));
        }
    }

    /// A `202` is a success and is not one of ours: the destination would not be on the
    /// panel yet, and the caller is about to report that it is.
    #[tokio::test]
    async fn an_unexpected_success_is_still_a_refusal() {
        let device = FakeDevice::answering([Ok(Response::status_only(202))]);

        let error = walkup_for(device)
            .expect(
                &Request::post_xml(DESTINATIONS_PATH, "<WalkupScanToComp/>"),
                &[status::CREATED],
            )
            .await
            .expect_err("202 is not 201");

        assert_eq!(
            error,
            WalkupError::Refused {
                device: Device::new("192.168.1.3"),
                request: format!("POST {DESTINATIONS_PATH}"),
                rejection: Rejection::Status(202),
            }
        );
    }

    /// The powered-off printer of the acceptance list: the failure names the device and
    /// what was tried, and reaches the client as `org.scanbus.Error.NotReachable`.
    #[tokio::test]
    async fn silence_is_not_reachable_and_names_what_was_tried() {
        let device =
            FakeDevice::answering([Err(TransportError::Io("connection refused".to_owned()))]);

        let error = walkup_for(device)
            .expect(&Request::get(DESTINATIONS_PATH), &[status::OK])
            .await
            .expect_err("nothing answered");

        assert!(!error.is_refusal());
        let message = error.to_string();
        assert!(message.contains("192.168.1.3"), "{message}");
        assert!(message.contains("GET /WalkupScanToComp"), "{message}");
        assert!(message.contains("connection refused"), "{message}");

        let scanner = ScannerId::from_backend(crate::ID, "192.168.1.3").unwrap();
        assert!(matches!(
            error.into_backend_error(&scanner),
            BackendError::NotReachable { .. }
        ));
    }

    /// A `200` carrying `document` as the collection.
    fn collection(document: &str) -> Response {
        Response::new(
            status::OK,
            vec![("Content-Type".into(), "text/xml".into())],
            document.as_bytes().to_vec(),
        )
    }

    /// The whole point of the sweep, in order: list, delete what was ours, then post.
    ///
    /// Posting first — or not deleting at all — is what puts `spectre` on the panel
    /// twice, one entry answering and one not, with nothing on the panel to tell them
    /// apart and no lease to clear the dead one.
    #[tokio::test]
    async fn registering_deletes_this_hosts_stale_entries_before_adding_one() {
        let device = FakeDevice::answering([
            Ok(collection(COLLECTION)),
            Ok(Response::status_only(status::NO_CONTENT)),
            Ok(created()),
        ]);
        let walkup = walkup_for(device.clone());

        let registration = walkup
            .register("spectre", &recorded_caps())
            .await
            .expect("the device agreed");

        let stale = parse_destinations(COLLECTION.as_bytes()).unwrap();
        let seen = device.seen();
        assert_eq!(seen.len(), 3, "{seen:?}");
        assert_eq!(seen[0].1.method(), Method::Get);
        assert_eq!(seen[0].1.target(), DESTINATIONS_PATH);
        assert_eq!(seen[1].1.method(), Method::Delete);
        assert_eq!(seen[1].1.target(), stale[0].uri());
        assert_eq!(seen[2].1.method(), Method::Post);
        assert_eq!(seen[2].1.target(), DESTINATIONS_PATH);
        assert_eq!(seen[2].1.body(), destination_document("spectre"));

        assert_eq!(registration.swept(), &stale[..1]);
        assert_eq!(
            registration.uri(),
            created().location().expect("the recorded 201 has one"),
        );
    }

    /// The reason the collection is parsed rather than searched for a string: everything
    /// in it that is not ours is another machine's, and a `DELETE` sent to one of those
    /// takes that machine off the panel with no way for it to notice.
    #[tokio::test]
    async fn no_other_machines_entry_is_touched() {
        let device = FakeDevice::answering([Ok(collection(COLLECTION)), Ok(created())]);
        let walkup = walkup_for(device.clone());

        let registration = walkup
            .register("desktop", &recorded_caps())
            .await
            .expect("the device agreed");

        let seen = device.seen();
        assert!(
            !seen
                .iter()
                .any(|(_, request)| request.method() == Method::Delete),
            "{seen:?}",
        );
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(registration.swept().is_empty());
    }

    /// Someone cleared the entry at the panel between the `GET` and the `DELETE`. The
    /// collection is shared, the outcome asked for was that the entry be gone, and it is.
    #[tokio::test]
    async fn a_stale_entry_that_had_already_gone_is_not_a_failure() {
        let device = FakeDevice::answering([
            Ok(collection(COLLECTION)),
            Ok(Response::status_only(status::NOT_FOUND)),
            Ok(created()),
        ]);

        let registration = walkup_for(device)
            .register("spectre", &recorded_caps())
            .await
            .expect("a destination that is already gone is gone");

        assert_eq!(registration.swept().len(), 1);
    }

    /// Carrying on after a `DELETE` the device refused would register the second entry
    /// beside the stale one, which is the state being swept for.
    #[tokio::test]
    async fn a_delete_the_device_refuses_stops_the_registration() {
        let device = FakeDevice::answering([
            Ok(collection(COLLECTION)),
            Ok(Response::status_only(500)),
            Ok(created()),
        ]);
        let walkup = walkup_for(device.clone());

        let error = walkup
            .register("spectre", &recorded_caps())
            .await
            .expect_err("the stale entry is still there");

        assert!(error.is_refusal(), "{error}");
        let seen = device.seen();
        assert!(
            !seen
                .iter()
                .any(|(_, request)| request.method() == Method::Post),
            "{seen:?}",
        );
    }

    /// A registration that cannot be reversed is not a registration to report as done:
    /// `stop_listening` would have nothing to send, and this host would sit on the panel
    /// until the next start swept it by name.
    #[tokio::test]
    async fn a_201_that_names_no_location_is_not_a_registration() {
        let empty = "<WalkupScanToCompDestinations/>";
        let device = FakeDevice::answering([
            Ok(collection(empty)),
            Ok(Response::status_only(status::CREATED)),
        ]);

        let error = walkup_for(device)
            .register("spectre", &recorded_caps())
            .await
            .expect_err("there is no URI to delete it by");

        assert!(!error.is_refusal(), "{error}");
        let message = error.to_string();
        assert!(message.contains("Location"), "{message}");
    }

    /// What the sweep claims, and what it must not.
    #[test]
    fn an_entry_is_this_hosts_by_hostname_whatever_its_case() {
        let destinations = parse_destinations(COLLECTION.as_bytes()).unwrap();

        assert!(destinations[0].registered_by("spectre"));
        assert!(destinations[0].registered_by("SPECTRE"));
        // The domain is dropped from the name that is registered, so it is not part of
        // the name that is matched either.
        assert!(!destinations[0].registered_by("spectre.lan"));
        assert!(!destinations[0].registered_by("laptop"));
    }

    /// An entry that published no `dd3:Hostname` belongs to something that did not say
    /// who it was — never to us, whatever this host ends up being called.
    #[test]
    fn an_entry_with_no_hostname_is_nobodys() {
        let anonymous = r#"<WalkupScanToCompDestinations>
  <WalkupScanToCompDestination>
    <dd:ResourceURI>/WalkupScanToComp/WalkupScanToCompDestinations/anonymous</dd:ResourceURI>
    <LinkType>Network</LinkType>
  </WalkupScanToCompDestination>
</WalkupScanToCompDestinations>"#;

        let destinations = parse_destinations(anonymous.as_bytes()).unwrap();

        assert_eq!(destinations.len(), 1, "{destinations:?}");
        assert!(!destinations[0].registered_by(""));
        assert!(!destinations[0].registered_by(FALLBACK_PANEL_NAME));
    }

    /// The document the device answered `201 Created` to, exactly as it went out.
    ///
    /// Written out here as one recorded string rather than reusing the template, so that
    /// a "tidy-up" of [`destination_document`] — a namespace hoisted to the root, the
    /// elements alphabetised, the root renamed to match the collection's own GET — fails
    /// here instead of on a printer, with a bare `400` and nothing to go on.
    const RECORDED_DOCUMENT: &str = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
        "<WalkupScanToComp ",
        "xmlns=\"http://www.hp.com/schemas/imaging/con/ledm/walkupscan/2010/09/28\" ",
        "xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" ",
        "xsi:schemaLocation=",
        "\"http://www.hp.com/schemas/imaging/con/ledm/walkupscan/2010/09/28 ",
        "WalkupScan.xsd\">",
        "<Hostname xmlns=\"http://www.hp.com/schemas/imaging/con/dictionaries/2009/04/06\">",
        "spectre</Hostname>",
        "<Name xmlns=\"http://www.hp.com/schemas/imaging/con/dictionaries/1.0/\">",
        "spectre</Name>",
        "<LinkType>Network</LinkType>",
        "</WalkupScanToComp>",
    );

    /// A collection shaped after the device's own `GET`: the element names and both
    /// dictionary prefixes are the recorded ones, the UUIDs and the second host are not.
    const COLLECTION: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<WalkupScanToCompDestinations
    xmlns="http://www.hp.com/schemas/imaging/con/ledm/walkupscan/2010/09/28"
    xmlns:dd="http://www.hp.com/schemas/imaging/con/dictionaries/1.0/"
    xmlns:dd3="http://www.hp.com/schemas/imaging/con/dictionaries/2009/04/06">
  <WalkupScanToCompDestination>
    <dd:ResourceURI>/WalkupScanToComp/WalkupScanToCompDestinations/1c8d4e40-dabb-1f08-aa30-644ed7fe04c4</dd:ResourceURI>
    <dd:Name>spectre</dd:Name>
    <dd3:Hostname>spectre</dd3:Hostname>
    <LinkType>Network</LinkType>
  </WalkupScanToCompDestination>
  <WalkupScanToCompDestination>
    <dd:ResourceURI>/WalkupScanToComp/WalkupScanToCompDestinations/7a1f0b22-9c04-1f08-b117-0242ac120002</dd:ResourceURI>
    <dd:Name>laptop</dd:Name>
    <dd3:Hostname>laptop</dd3:Hostname>
    <LinkType>Network</LinkType>
  </WalkupScanToCompDestination>
</WalkupScanToCompDestinations>
"#;

    /// The capability document, carrying the two values the device was recorded
    /// publishing: `MaxNetworkDestinations` 15 and a 60-second `UserActionTimeout`.
    const CAPS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<WalkupScanToCompCaps
    xmlns="http://www.hp.com/schemas/imaging/con/ledm/walkupscan/2010/09/28"
    xmlns:dd="http://www.hp.com/schemas/imaging/con/dictionaries/1.0/">
  <MaxNetworkDestinations>15</MaxNetworkDestinations>
  <UserActionTimeout>60</UserActionTimeout>
</WalkupScanToCompCaps>
"#;

    /// A collection holding `count` entries, none of them this host's.
    fn foreign_collection(count: usize) -> Response {
        let entries = (0..count)
            .map(|index| {
                format!(
                    "<WalkupScanToCompDestination>\
                     <dd:ResourceURI>{DESTINATIONS_PATH}/host-{index}</dd:ResourceURI>\
                     <dd3:Hostname>host-{index}</dd3:Hostname>\
                     </WalkupScanToCompDestination>"
                )
            })
            .collect::<String>();
        collection(&format!(
            "<WalkupScanToCompDestinations>{entries}</WalkupScanToCompDestinations>"
        ))
    }

    #[test]
    fn the_limit_is_read_off_the_capability_document() {
        let limits = parse_capabilities(CAPS.as_bytes()).expect("the recorded document");

        assert_eq!(limits.max_network_destinations(), Some(15));
    }

    /// A firmware that stopped publishing the element must not take every host off its
    /// own panel: an unknown limit is not a full device, and the device can still refuse
    /// the `POST` itself.
    #[test]
    fn a_capability_document_without_the_element_names_no_limit() {
        let limits = parse_capabilities(
            b"<WalkupScanToCompCaps><UserActionTimeout>60</UserActionTimeout>\
              </WalkupScanToCompCaps>",
        )
        .expect("a document, just not one with a limit in it");

        assert_eq!(limits.max_network_destinations(), None);
    }

    /// Present and unreadable is the opposite case: this module is misreading the
    /// document, and a limit guessed from one is how an empty device gets reported full.
    #[test]
    fn a_limit_that_is_not_a_number_is_a_malformed_document() {
        let error = parse_capabilities(
            b"<WalkupScanToCompCaps><MaxNetworkDestinations>many\
              </MaxNetworkDestinations></WalkupScanToCompCaps>",
        )
        .expect_err("\"many\" is not a count of destinations");

        assert!(error.reason().contains("MaxNetworkDestinations"), "{error}");
    }

    /// The USB case. `/WalkupScanToComp` is not served at all, `hpssd` reports
    /// `scanWaitingForPC` off the USB status channel anyway, and there is nothing here
    /// to fail.
    #[tokio::test]
    async fn a_device_that_serves_no_capabilities_registers_nothing() {
        let device = FakeDevice::answering([Ok(Response::status_only(status::NOT_FOUND))]);
        let walkup = walkup_for(device.clone());

        assert_eq!(walkup.limits().await.expect("404 is an answer"), None);
        assert_eq!(
            walkup.install("spectre").await.expect("nothing to do"),
            None
        );

        let seen = device.seen();
        assert!(
            seen.iter().all(
                |(_, request)| request.method() == Method::Get && request.target() == CAPS_PATH
            ),
            "nothing may be written to a device that has no collection: {seen:?}",
        );
    }

    /// A device that answers `500` for its own capability document is not a USB printer,
    /// and treating it as one would register nothing and say nothing.
    #[tokio::test]
    async fn a_capability_document_the_device_refuses_is_not_the_usb_case() {
        let device = FakeDevice::answering([Ok(Response::status_only(500))]);

        let error = walkup_for(device)
            .install("spectre")
            .await
            .expect_err("500 is not 404");

        assert!(error.is_refusal(), "{error}");
    }

    /// The whole flow against a device that has room: caps, list, sweep, post.
    #[tokio::test]
    async fn installing_reads_the_capabilities_before_registering() {
        let device = FakeDevice::answering([
            Ok(collection(CAPS)),
            Ok(collection(COLLECTION)),
            Ok(Response::status_only(status::NO_CONTENT)),
            Ok(created()),
        ]);
        let walkup = walkup_for(device.clone());

        let registration = walkup
            .install("spectre")
            .await
            .expect("the device agreed")
            .expect("this one has a collection");

        let seen = device.seen();
        assert_eq!(seen.len(), 4, "{seen:?}");
        assert_eq!(seen[0].1.target(), CAPS_PATH);
        assert_eq!(seen[3].1.method(), Method::Post);
        assert_eq!(
            registration.uri(),
            created().location().expect("the recorded 201 has one"),
        );
    }

    /// Fifteen other machines. Nothing is posted — the device's own answer to a full
    /// collection is a bare status with no entity, which would reach the user as "the
    /// printer refused the document" and send them after the XML.
    #[tokio::test]
    async fn a_full_collection_names_the_limit_and_what_to_do_about_it() {
        let device = FakeDevice::answering([Ok(collection(CAPS)), Ok(foreign_collection(15))]);
        let walkup = walkup_for(device.clone());

        let error = walkup
            .install("spectre")
            .await
            .expect_err("there is no room for this host");

        let message = error.to_string();
        assert!(message.contains("15"), "{message}");
        assert!(message.contains("Scan to Computer"), "{message}");
        assert!(!error.is_refusal(), "the device was never asked: {error}");

        let seen = device.seen();
        assert!(
            !seen
                .iter()
                .any(|(_, request)| request.method() == Method::Post),
            "{seen:?}",
        );

        let scanner = ScannerId::from_backend(crate::ID, "192.168.1.3").unwrap();
        assert!(matches!(
            error.into_backend_error(&scanner),
            BackendError::Other(_)
        ));
    }

    /// The fifteenth entry being *ours* is the crash-loop case, and it is room: the sweep
    /// takes it back before the count is made, so a host that leaked its way to a full
    /// collection recovers on its own rather than sending the user to the panel.
    #[tokio::test]
    async fn this_hosts_own_entries_are_swept_before_the_collection_is_counted() {
        let mut full = (0..14)
            .map(|index| {
                format!(
                    "<WalkupScanToCompDestination>\
                     <dd:ResourceURI>{DESTINATIONS_PATH}/host-{index}</dd:ResourceURI>\
                     <dd3:Hostname>host-{index}</dd3:Hostname>\
                     </WalkupScanToCompDestination>"
                )
            })
            .collect::<String>();
        full.push_str(&format!(
            "<WalkupScanToCompDestination>\
                 <dd:ResourceURI>{DESTINATIONS_PATH}/stale</dd:ResourceURI>\
                 <dd3:Hostname>spectre</dd3:Hostname>\
                 </WalkupScanToCompDestination>"
        ));
        let device = FakeDevice::answering([
            Ok(collection(CAPS)),
            Ok(collection(&format!(
                "<WalkupScanToCompDestinations>{full}</WalkupScanToCompDestinations>"
            ))),
            Ok(Response::status_only(status::NO_CONTENT)),
            Ok(created()),
        ]);
        let walkup = walkup_for(device.clone());

        let registration = walkup
            .install("spectre")
            .await
            .expect("the stale entry made the room")
            .expect("this one has a collection");

        assert_eq!(registration.swept().len(), 1);
        let seen = device.seen();
        assert_eq!(seen[2].1.method(), Method::Delete);
        assert_eq!(seen[2].1.target(), format!("{DESTINATIONS_PATH}/stale"));
        assert_eq!(seen[3].1.method(), Method::Post);
    }

    /// An unknown limit is not a full device: the `POST` goes out and the device answers
    /// for itself.
    #[tokio::test]
    async fn a_device_that_published_no_limit_is_never_full() {
        let device = FakeDevice::answering([
            Ok(collection("<WalkupScanToCompDestinations/>")),
            Ok(created()),
        ]);

        walkup_for(device)
            .register("spectre", &Limits::default())
            .await
            .expect("nothing here says there is no room");
    }

    #[test]
    fn a_network_uri_gives_the_host_to_dial() {
        for (uri, expected) in [
            (
                "hp:/net/Officejet_250?ip=192.168.1.3&queue=false",
                "192.168.1.3",
            ),
            (
                "hp:/net/Officejet_250?zc=HPF430B9BF1CAD.local.",
                "HPF430B9BF1CAD.local",
            ),
            ("hp:/net/Officejet_250?hostname=printer.lan", "printer.lan"),
            // `ip` first: a literal address needs no resolver, and the resolver is where
            // this fails on a laptop whose mDNS stack is asleep.
            (
                "hp:/net/X?zc=HP.local.&hostname=printer.lan&ip=192.168.1.3",
                "192.168.1.3",
            ),
        ] {
            assert_eq!(
                Device::from_device_uri(uri),
                Some(Device::new(expected)),
                "device_uri {uri:?}",
            );
        }
    }

    /// A USB device has nothing to dial, and serves no `/WalkupScanToComp` either.
    #[test]
    fn a_usb_uri_names_no_device_to_register_with() {
        for uri in [
            "hp:/usb/Officejet_250?serial=CN12345678",
            "hp:/usb/Officejet_250?devnum=1",
            "hp:/net/Officejet_250",
        ] {
            assert_eq!(Device::from_device_uri(uri), None, "device_uri {uri:?}");
        }
    }

    #[test]
    fn the_registration_document_is_the_one_the_device_accepted() {
        assert_eq!(
            String::from_utf8(destination_document("spectre")).unwrap(),
            RECORDED_DOCUMENT,
        );
    }

    /// Not a hostname anyone has, but the value is read off the filesystem and the
    /// failure it would cause — a bare `400` — says nothing about where it came from.
    #[test]
    fn a_name_with_markup_in_it_cannot_end_an_element_early() {
        let document = String::from_utf8(destination_document("a<b&c")).unwrap();
        assert!(document.contains(">a&lt;b&amp;c</Hostname>"), "{document}");
        assert!(document.ends_with("</WalkupScanToComp>"), "{document}");
    }

    /// Every case here is an ordinary hostname.
    #[test]
    fn the_panel_name_is_the_machine_name_without_its_domain() {
        for (hostname, expected) in [
            ("spectre", "spectre"),
            ("desktop.office.example.com", "desktop"),
            ("workstation-04.lan", "workstation-04"),
            // Long, punctuated, and left alone: HP's field is not Brother's.
            ("a-very-long-hostname-indeed", "a-very-long-hostname-indeed"),
            ("", FALLBACK_PANEL_NAME),
            ("   ", FALLBACK_PANEL_NAME),
            (".lan", FALLBACK_PANEL_NAME),
        ] {
            assert_eq!(panel_name_from(hostname), expected, "hostname {hostname:?}");
        }
    }

    #[test]
    fn the_collection_pairs_each_uri_with_the_host_that_registered_it() {
        let destinations = parse_destinations(COLLECTION.as_bytes()).unwrap();

        assert_eq!(destinations.len(), 2, "{destinations:?}");
        assert_eq!(destinations[0].hostname(), "spectre");
        assert_eq!(
            destinations[0].uri(),
            "/WalkupScanToComp/WalkupScanToCompDestinations/1c8d4e40-dabb-1f08-aa30-644ed7fe04c4",
        );
        assert_eq!(destinations[1].hostname(), "laptop");
        assert!(
            destinations[1].uri().ends_with("0242ac120002"),
            "{destinations:?}"
        );
    }

    /// The prefixes are the document's own choice, so the parse cannot depend on them.
    #[test]
    fn the_same_collection_under_different_prefixes_reads_the_same() {
        let renamed = COLLECTION
            .replace("xmlns:dd3=", "xmlns:x=")
            .replace("dd3:", "x:")
            .replace("xmlns:dd=", "xmlns:y=")
            .replace("dd:", "y:");

        assert_eq!(
            parse_destinations(renamed.as_bytes()).unwrap(),
            parse_destinations(COLLECTION.as_bytes()).unwrap(),
        );
    }

    /// An empty collection is the symptom this whole module exists to fix, and it is not
    /// a failure to report — it is a device nobody has registered with yet.
    #[test]
    fn an_empty_collection_is_empty_and_not_an_error() {
        let empty = r#"<?xml version="1.0" encoding="UTF-8"?>
<WalkupScanToCompDestinations
    xmlns="http://www.hp.com/schemas/imaging/con/ledm/walkupscan/2010/09/28"/>
"#;
        assert_eq!(parse_destinations(empty.as_bytes()).unwrap(), Vec::new());
    }

    /// An entry with no address is one the sweep could not delete anyway, and returning
    /// it with an empty URI is how a `DELETE` ends up addressed at the collection itself.
    #[test]
    fn an_entry_without_a_resource_uri_is_not_returned() {
        let collection = r#"<WalkupScanToCompDestinations>
  <WalkupScanToCompDestination>
    <dd3:Hostname>spectre</dd3:Hostname>
  </WalkupScanToCompDestination>
  <WalkupScanToCompDestination>
    <dd:ResourceURI>/WalkupScanToComp/WalkupScanToCompDestinations/one</dd:ResourceURI>
    <dd3:Hostname>laptop</dd3:Hostname>
  </WalkupScanToCompDestination>
</WalkupScanToCompDestinations>"#;

        let destinations = parse_destinations(collection.as_bytes()).unwrap();
        assert_eq!(destinations.len(), 1, "{destinations:?}");
        assert_eq!(destinations[0].hostname(), "laptop");
    }

    /// The reader hands back a name that had to be escaped in pieces — a text run, the
    /// reference, another text run — so a value assembled by overwriting instead of
    /// appending would keep only the last piece, and the sweep would then match a
    /// truncated hostname against ours and leave the real entry on the panel.
    #[test]
    fn a_hostname_that_had_to_be_escaped_comes_back_whole() {
        let collection = r#"<WalkupScanToCompDestinations>
  <WalkupScanToCompDestination>
    <dd:ResourceURI>/WalkupScanToComp/WalkupScanToCompDestinations/one</dd:ResourceURI>
    <dd3:Hostname>black &amp; white &#40;spare&#41;</dd3:Hostname>
  </WalkupScanToCompDestination>
</WalkupScanToCompDestinations>"#;

        let destinations = parse_destinations(collection.as_bytes()).unwrap();
        assert_eq!(destinations[0].hostname(), "black & white (spare)");
    }

    /// Pretty-printing is the device's business, not ours: a value laid out over its own
    /// line is the same value, and `"\n    spectre\n  "` matches no hostname at all.
    #[test]
    fn a_value_on_its_own_line_is_the_value_without_the_layout() {
        let collection = r#"<WalkupScanToCompDestinations>
  <WalkupScanToCompDestination>
    <dd:ResourceURI>
      /WalkupScanToComp/WalkupScanToCompDestinations/one
    </dd:ResourceURI>
    <dd3:Hostname>
      spectre
    </dd3:Hostname>
  </WalkupScanToCompDestination>
</WalkupScanToCompDestinations>"#;

        let destinations = parse_destinations(collection.as_bytes()).unwrap();
        assert_eq!(
            destinations[0].uri(),
            "/WalkupScanToComp/WalkupScanToCompDestinations/one"
        );
        assert_eq!(destinations[0].hostname(), "spectre");
    }

    #[test]
    fn a_document_that_is_not_the_collection_is_malformed() {
        let broken = "<WalkupScanToCompDestination><dd:ResourceURI>/one</dd3:Hostname>";
        assert!(parse_destinations(broken.as_bytes()).is_err());
    }

    #[test]
    fn a_get_carries_no_entity_headers() {
        let wire = String::from_utf8(serialise(
            &Device::new("192.168.1.3"),
            &Request::get(DESTINATIONS_PATH),
        ))
        .unwrap();

        let start = "GET /WalkupScanToComp/WalkupScanToCompDestinations HTTP/1.1\r\n";
        assert!(wire.starts_with(start), "{wire}");
        assert!(wire.contains("\r\nHost: 192.168.1.3\r\n"), "{wire}");
        assert!(wire.contains("\r\nConnection: close\r\n"), "{wire}");
        assert!(!wire.contains("Content-Type"), "{wire}");
        assert!(!wire.contains("Content-Length"), "{wire}");
        assert!(wire.ends_with("\r\n\r\n"), "{wire}");
    }

    #[test]
    fn a_post_carries_the_media_type_the_device_insists_on_and_a_length() {
        let body = destination_document("spectre");
        let wire = serialise(
            &Device::new("192.168.1.3").with_port(8080),
            &Request::post_xml(DESTINATIONS_PATH, body.clone()),
        );
        let wire = String::from_utf8(wire).unwrap();

        assert!(wire.contains("\r\nHost: 192.168.1.3:8080\r\n"), "{wire}");
        assert!(wire.contains("\r\nContent-Type: text/xml\r\n"), "{wire}");
        assert!(
            wire.contains(&format!("\r\nContent-Length: {}\r\n", body.len())),
            "{wire}",
        );
        assert!(wire.ends_with(&String::from_utf8(body).unwrap()), "{wire}");
    }

    /// A one-shot HTTP server on loopback: reads one request, writes `answer`, and either
    /// closes or holds the connection open the way a keep-alive device would.
    async fn serving(answer: Vec<u8>, close: bool) -> (u16, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            stream.write_all(&answer).await.unwrap();
            if close {
                stream.shutdown().await.unwrap();
            } else {
                sleep(Duration::from_secs(30)).await;
            }
            request
        });
        (port, served)
    }

    /// One request, framed the way the transport frames what it sends.
    async fn read_request(stream: &mut TcpStream) -> Vec<u8> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            if let Some(end) = find(&buffer, b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if buffer.len() >= end + 4 + length {
                    return buffer;
                }
            }
            let read = stream.read(&mut chunk).await.unwrap();
            if read == 0 {
                return buffer;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
    }

    fn device_on(port: u16) -> Device {
        Device::new("127.0.0.1").with_port(port)
    }

    #[tokio::test]
    async fn a_registration_goes_out_and_its_201_comes_back() {
        let location = concat!(
            "/WalkupScanToComp/WalkupScanToCompDestinations/",
            "1c8d4e40-dabb-1f08-aa30-644ed7fe04c4",
        );
        let answer =
            format!("HTTP/1.1 201 Created\r\nContent-Length: 0\r\nLocation: {location}\r\n\r\n");
        let (port, served) = serving(answer.into_bytes(), true).await;

        let body = destination_document("spectre");
        let request = Request::post_xml(DESTINATIONS_PATH, body.clone());
        let response = TcpHttp::default()
            .exchange(&device_on(port), &request)
            .await
            .unwrap();

        assert_eq!(response.status(), status::CREATED);
        assert_eq!(response.location(), Some(location));
        assert!(response.body().is_empty());

        let sent = served.await.unwrap();
        assert!(sent.ends_with(&body), "{}", String::from_utf8_lossy(&sent));
    }

    /// A collection served without a length, chunk by chunk, is the shape an embedded
    /// server that builds its answer element by element produces.
    #[tokio::test]
    async fn a_chunked_collection_is_reassembled() {
        let (first, second) = COLLECTION.split_at(COLLECTION.len() / 2);
        let answer = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: text/xml\r\n\
             Transfer-Encoding: chunked\r\n\
             \r\n\
             {:x}\r\n{first}\r\n{:x}\r\n{second}\r\n0\r\n\r\n",
            first.len(),
            second.len(),
        );
        let (port, _served) = serving(answer.into_bytes(), true).await;

        let response = TcpHttp::default()
            .exchange(&device_on(port), &Request::get(DESTINATIONS_PATH))
            .await
            .unwrap();

        assert_eq!(response.status(), status::OK);
        assert_eq!(response.body(), COLLECTION.as_bytes());
        assert_eq!(parse_destinations(response.body()).unwrap().len(), 2);
    }

    /// The teardown case: a `204` has no entity whatever else it says, and a device that
    /// keeps the connection open must not turn every `DELETE` into a timeout.
    #[tokio::test]
    async fn a_204_is_not_waited_out() {
        let answer = "HTTP/1.1 204 No Content\r\nConnection: keep-alive\r\n\r\n";
        let (port, _served) = serving(answer.as_bytes().to_vec(), false).await;

        let deleted = timeout(
            Duration::from_secs(5),
            TcpHttp::default().exchange(
                &device_on(port),
                &Request::delete("/WalkupScanToComp/WalkupScanToCompDestinations/one"),
            ),
        )
        .await
        .expect("a 204 must not wait for a close");

        assert_eq!(deleted.unwrap().status(), status::NO_CONTENT);
    }

    /// A response with neither a length nor chunks: the close is what ends it.
    #[tokio::test]
    async fn a_body_that_only_the_close_delimits_is_still_read() {
        let answer = format!("HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\n\r\n{COLLECTION}");
        let (port, _served) = serving(answer.into_bytes(), true).await;

        let response = TcpHttp::default()
            .exchange(&device_on(port), &Request::get(DESTINATIONS_PATH))
            .await
            .unwrap();

        assert_eq!(response.body(), COLLECTION.as_bytes());
    }

    #[tokio::test]
    async fn a_device_that_is_not_there_is_an_io_failure_and_not_a_refusal() {
        // Bound and dropped: a port nothing is listening on, without guessing one.
        let closed = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);

        let error = TcpHttp::default()
            .exchange(&device_on(port), &Request::get(CAPS_PATH))
            .await
            .expect_err("nothing is listening");

        assert!(matches!(error, TransportError::Io(_)), "{error:?}");
    }

    /// The `Walkup` end of it: a collection off a real socket, parsed.
    #[tokio::test]
    async fn destinations_come_back_parsed() {
        let answer = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\r\n{COLLECTION}",
            COLLECTION.len(),
        );
        let (port, _served) = serving(answer.into_bytes(), true).await;

        let walkup = Walkup::new(device_on(port), Arc::new(TcpHttp::default()));
        let destinations = walkup.destinations().await.unwrap();

        assert_eq!(destinations.len(), 2, "{destinations:?}");
        assert_eq!(destinations[0].hostname(), "spectre");
    }

    /// A `200` whose entity is not a collection is neither silence nor a refusal, and it
    /// must not be reported as either.
    #[tokio::test]
    async fn an_unreadable_collection_says_so() {
        let device = FakeDevice::answering([Ok(Response::new(
            status::OK,
            vec![("Content-Type".into(), "text/xml".into())],
            b"<WalkupScanToCompDestination></nonsense>".to_vec(),
        ))]);

        let error = walkup_for(device)
            .destinations()
            .await
            .expect_err("that is not a collection");

        assert!(matches!(error, WalkupError::Malformed { .. }), "{error:?}");
        assert!(!error.is_refusal());

        let scanner = ScannerId::from_backend(crate::ID, "192.168.1.3").unwrap();
        assert!(matches!(
            error.into_backend_error(&scanner),
            BackendError::Other(_)
        ));
    }
}
