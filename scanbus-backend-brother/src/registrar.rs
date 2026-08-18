//! Appearing on the printer's panel, and staying there.
//!
//! [`skey::register`](crate::skey::register) builds the registration string and the
//! `SetRequest` that carries it; this module is what actually sends it, to one device,
//! for as long as the host is supposed to be selectable under *Scan → to PC*.
//!
//! # Registering is a task, not a call
//!
//! The registration carries `DURATION=360` — a lease in seconds — so the panel entry
//! disappears on its own unless something renews it. That is leaned on rather than
//! worked around ([`brother-skeyless-backend.md`] §2.2): a daemon that crashes leaves no
//! dead entry for the next person to select, and a machine that suspends or changes
//! network stops appearing rather than appearing and doing nothing.
//!
//! Refresh is therefore at **half the lease** ([`refresh_delay`]), not at the lease: one
//! lost datagram then costs a retry rather than a visible outage on the LCD.
//!
//! # Stopping sends nothing
//!
//! [`Registrar::stop`] aborts the refresh task and that is the whole of deregistration —
//! the entry lapses within one `DURATION`. It is tempting to send a teardown instead, and
//! there is nothing to send: the vendor daemon's symbol table has `unregister_usb_scanner`
//! and no network counterpart, and `registerpc.c` has no path that tells a device to
//! forget a host. A teardown would be an invention, and an invention that then has to
//! arrive — over UDP, from a process that may be being killed. The lease already covers
//! the case where it would not.
//!
//! # Waiting two seconds for an answer
//!
//! [`RESPONSE_TIMEOUT`] is the vendor's: `snmp_recv` at `0x404b8b` builds a `timeval` of
//! `{ tv_sec = 2, tv_usec = 0 }` and `select`s on it exactly once, with no retry. We keep
//! the wait and add the retry, because unlike `brscan-skey` we are not registering once
//! at startup but every three minutes forever.
//!
//! # Where the device is told to send the key press
//!
//! `HOST=` is the one field here that can be silently wrong, and being wrong costs the
//! worst failure this backend has: the host appears on the panel, the user selects it,
//! and nothing whatsoever happens. It must be the address of the interface that routes to
//! *that device* — which is what [`RoutedHost`] asks the kernel, by `connect`ing a UDP
//! socket and reading `local_addr`. That costs no packet: a `connect` on a datagram socket
//! is a route lookup and a bind, not a handshake.
//!
//! Enumerating the machine's interfaces and taking the first non-loopback address is the
//! obvious alternative and is wrong on any laptop: a VPN, a docker bridge or a second NIC
//! all produce addresses the printer cannot reach, and which one comes first is an
//! ordering accident. The routing table already holds the answer, per device.
//!
//! It is re-derived on **every** refresh rather than once at [`Registrar::start`], because
//! the address is exactly what a suspend-and-resume, a VPN coming up or a move between
//! networks changes. Re-deriving it makes that self-correcting within one lease; caching
//! it makes the entry stay on the panel pointing at an address this host no longer has.
//!
//! # The name on the panel
//!
//! [`panel_name`] is what the user reads off the LCD and selects. It comes from the
//! machine's hostname, filtered to the ≤15 alphanumeric characters Brother documents for
//! a scan destination — `workstation-04.lan` becomes `workstation04` — and is logged once
//! at startup, because a name the user cannot recognise on the panel is otherwise only
//! discoverable by walking to the printer.
//!
//! # Cancellation
//!
//! Every await in the refresh task is a cancellation point and none of them owns anything
//! that outlives the process: the only state a round creates is on the *device*, and it is
//! a lease that expires by itself. Dropping the task therefore stops refreshing and does
//! nothing else — no half-written state, no packet owed to anybody — which is what
//! [`ScannerBackend`](scanbus_core::ScannerBackend)'s contract asks of the listener this
//! will hang from (5.9).
//!
//! [`brother-skeyless-backend.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/brother-skeyless-backend.md

use std::collections::BTreeSet;
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use scanbus_core::{BackendError, ScannerId};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};

use crate::skey::function::Function;
use crate::skey::register::{
    DEFAULT_DURATION, LISTENER_PORT, RegisterError, Registration, UserName, probe_request,
};
use crate::skey::snmp::{self, ErrorStatus, Message, PduKind, SnmpError};

/// How long a device is given to answer one `SetRequest`, from `snmp_recv` at `0x404b8b`.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Longest datagram this module will read. SNMP responses to a one-varbind request are
/// tens of bytes; `snmp_recv` hands `recvfrom` a 2 KiB buffer and so do we.
const RECV_BUFFER: usize = 2048;

/// When to renew a lease of `duration`: at half of it.
///
/// Half rather than "shortly before expiry" so that a single dropped request — a UDP
/// datagram to a printer that is busy scanning — is invisible: there is a whole second
/// attempt inside the lease before the entry could vanish from the panel.
pub fn refresh_delay(duration: Duration) -> Duration {
    duration / 2
}

/// When to try again after a refresh round failed.
///
/// Bounded on both sides on purpose. A printer that is unplugged must not be retried in
/// a tight loop — every attempt costs a [`RESPONSE_TIMEOUT`] wait and a log line — and it
/// must be re-registered within one lease of coming back, which a 12th-of-a-lease step
/// gives with room to spare.
fn retry_delay(duration: Duration) -> Duration {
    (duration / 12).clamp(Duration::from_secs(5), Duration::from_secs(60))
}

/// Where the machine's name is, on a running Linux system.
///
/// `/proc` first because it is the live value: `/etc/hostname` is what the machine was
/// called at boot, and a `hostnamectl set-hostname` since then has not changed it.
const HOSTNAME_FILES: [&str; 2] = ["/proc/sys/kernel/hostname", "/etc/hostname"];

/// What this host is called when the hostname yields nothing usable — a machine called
/// `-`, or one whose name is entirely non-ASCII. Recognisable on a panel, and true.
const FALLBACK_PANEL_NAME: &str = "scanbus";

/// The name this host appears under on the panel, derived from the machine's hostname.
///
/// Derived rather than configured because the user has to *recognise* it on a printer's
/// LCD, and the machine's own name is the only thing they already know. The rule it has
/// to fit is Brother's — at most [`MAX_USER_LEN`](crate::skey::register::MAX_USER_LEN)
/// alphanumeric characters — which is why this is a filter and a truncation rather than a
/// copy: `workstation-04.lan` is a perfectly ordinary hostname, and neither its `-`, nor
/// its `.`, nor its length is acceptable in `USER=`.
///
/// The domain goes first, before the truncation: `desktop.office.example.com` should
/// appear as `desktop`, not as `desktopofficee`.
///
/// Computed and **logged once**, on the first call. The log line is not decoration: it is
/// the only place the user can find out what to look for on the panel without walking to
/// the printer, and truncation makes the answer non-obvious exactly when the hostname is
/// long.
pub fn panel_name() -> UserName {
    static NAME: OnceLock<UserName> = OnceLock::new();
    NAME.get_or_init(|| {
        let hostname = read_hostname();
        let name = panel_name_from(hostname.as_deref().unwrap_or(""));
        info!(
            panel_name = %name,
            hostname = hostname.as_deref().unwrap_or("<unreadable>"),
            "this host will appear under Scan to PC on Brother panels under this name",
        );
        name
    })
    .clone()
}

/// [`panel_name`]'s rule, without the filesystem — every case in it is a real hostname.
fn panel_name_from(hostname: &str) -> UserName {
    let short = hostname.split('.').next().unwrap_or_default();
    UserName::sanitised(short)
        .or_else(|| UserName::sanitised(hostname))
        .unwrap_or_else(|| {
            UserName::new(FALLBACK_PANEL_NAME).expect("the fallback name is 7 alphanumerics")
        })
}

fn read_hostname() -> Option<String> {
    HOSTNAME_FILES.iter().find_map(|path| {
        let name = std::fs::read_to_string(path).ok()?;
        let name = name.trim().to_owned();
        (!name.is_empty()).then_some(name)
    })
}

/// Which of this machine's addresses a given device should be told to answer to.
///
/// A trait because the answer is a property of the machine's routing table, which a test
/// can neither arrange nor predict — and because a network changing under a running lease
/// is the case that has to be tested and cannot be staged on real hardware.
#[async_trait]
pub trait HostAddress: fmt::Debug + Send + Sync + 'static {
    /// The local address to advertise in `HOST=` for `device`.
    async fn towards(&self, device: Ipv4Addr) -> Result<Ipv4Addr, TransportError>;
}

/// The kernel's own answer, from the routing table, for the cost of a socket.
///
/// `connect` on a UDP socket sends nothing — it picks the route, binds the source address
/// and remembers the peer — so `local_addr` afterwards is precisely "the address this
/// machine would speak to that device from". The printer therefore hears about the
/// interface that can reach it, whatever else is up on the machine.
#[derive(Debug, Clone, Copy, Default)]
pub struct RoutedHost;

#[async_trait]
impl HostAddress for RoutedHost {
    async fn towards(&self, device: Ipv4Addr) -> Result<Ipv4Addr, TransportError> {
        let io = |error: std::io::Error| TransportError::Io(error.to_string());

        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
            .await
            .map_err(io)?;
        socket
            .connect(SocketAddrV4::new(device, snmp::SNMP_PORT))
            .await
            .map_err(io)?;

        match socket.local_addr().map_err(io)? {
            SocketAddr::V4(local) if local.ip().is_unspecified() => Err(TransportError::Io(
                format!("the route to {device} left the source address unbound"),
            )),
            SocketAddr::V4(local) => Ok(*local.ip()),
            // Not reachable through the bind above; a Brother registration has nowhere to
            // put an IPv6 address, so it is a failure and not something to truncate.
            SocketAddr::V6(local) => Err(TransportError::Io(format!(
                "the route to {device} came back as {local}, which HOST= cannot carry"
            ))),
        }
    }
}

/// One address, whatever the routing table says.
///
/// For a caller that knows better than the kernel — and for tests, which is where the
/// distinction between "the address changed" and "the registration changed" is checked.
#[derive(Debug, Clone, Copy)]
pub struct FixedHost(pub Ipv4Addr);

#[async_trait]
impl HostAddress for FixedHost {
    async fn towards(&self, _device: Ipv4Addr) -> Result<Ipv4Addr, TransportError> {
        Ok(self.0)
    }
}

/// What carries one SNMP exchange to a device.
///
/// A trait, so that the refresh loop above it can be tested against a printer that
/// refuses, a printer that is unplugged, and a lease that runs for a simulated hour,
/// none of which is reproducible on hardware in a test suite. [`UdpSnmp`] is the only
/// implementation that opens a socket.
#[async_trait]
pub trait SnmpTransport: fmt::Debug + Send + Sync + 'static {
    /// Send `request` to `device` and return the answer to *that* request.
    async fn exchange(
        &self,
        device: SocketAddrV4,
        request: &Message,
    ) -> Result<Message, TransportError>;
}

/// Everything that can go wrong between the encoder and the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// Nothing arrived in time. Says nothing about whether the device would have
    /// accepted the registration — see [`RegistrarError::Unreachable`].
    Timeout(Duration),
    /// The socket failed: no route to host, or a local address that cannot be bound.
    Io(String),
    /// A message this module cannot have produced. Ours, not the device's.
    Encode(SnmpError),
    /// Something answered and it was not an SNMP message we can read.
    Malformed(SnmpError),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout(after) => write!(f, "no answer within {:?}", after),
            Self::Io(error) => write!(f, "socket error: {error}"),
            Self::Encode(error) => write!(f, "could not encode the request: {error}"),
            Self::Malformed(error) => write!(f, "unreadable answer: {error}"),
        }
    }
}

impl std::error::Error for TransportError {}

/// Why a registration did not take.
///
/// The three network variants are kept apart because they mean three different things to
/// the user: [`Unreachable`](Self::Unreachable) is a printer that is off or on another
/// network, [`Refused`](Self::Refused) is a *model* that does not do scan-to-PC over
/// SNMP — the supported degraded path, zero buttons and a perfectly good pull scanner —
/// and [`Unexpected`](Self::Unexpected) is something on the network answering for the
/// printer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrarError {
    /// The registration string could not be built: a name the device would reject, or a
    /// lease its field cannot express. Never reaches the network.
    Registration(RegisterError),
    /// The device did not answer.
    Unreachable {
        device: Ipv4Addr,
        source: TransportError,
    },
    /// The device answered, and said no.
    ///
    /// `function` is the registration that was refused, or `None` when it was the
    /// read-only [`probe_scan_to_pc`] that came back refused — a device that will not
    /// even let the OID be *read* is not going to accept a write to it.
    Refused {
        device: Ipv4Addr,
        function: Option<Function>,
        status: ErrorStatus,
    },
    /// The device answered something that is not an answer to what we asked.
    Unexpected { device: Ipv4Addr, reason: String },
}

impl fmt::Display for RegistrarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registration(error) => write!(f, "{error}"),
            Self::Unreachable { device, source } => {
                write!(f, "{device} did not answer the registration: {source}")
            }
            Self::Refused {
                device,
                function,
                status,
            } => {
                match function {
                    Some(function) => write!(f, "{device} refused to register {function}")?,
                    None => write!(f, "{device} refused to read the scan-key OID")?,
                }
                write!(
                    f,
                    " with {status}: this model does not appear to support scan-to-PC over SNMP"
                )
            }
            Self::Unexpected { device, reason } => {
                write!(f, "{device} answered the registration with {reason}")
            }
        }
    }
}

impl std::error::Error for RegistrarError {}

impl RegistrarError {
    /// True when the device answered and said no — as opposed to not answering.
    ///
    /// The distinction is the whole of the degraded path: a refusal is a statement about
    /// the *model*, permanent and worth recording against the scanner, while silence is a
    /// statement about right now and worth nothing but a retry.
    pub const fn is_refusal(&self) -> bool {
        matches!(self, Self::Refused { .. })
    }

    /// The scanbus error to report for this failure.
    ///
    /// The two network cases map to two different D-Bus errors on purpose, because they
    /// ask the user for two different things:
    ///
    /// - [`Unreachable`](Self::Unreachable) → [`BackendError::NotReachable`], i.e.
    ///   `org.scanbus.Error.NotReachable`: switch the printer on, or plug it back in.
    /// - [`Refused`](Self::Refused) → [`BackendError::Unsupported`], i.e.
    ///   `org.freedesktop.DBus.Error.NotSupported`: nothing to fix — this model does not
    ///   do scan-to-PC over SNMP, and no amount of retrying will change that. It is also
    ///   the *only* one of these that means that, which is why it does not go through
    ///   `NotReachable` with a longer message.
    ///
    /// The remaining two are local or nonsensical rather than device conditions, so they
    /// land on [`BackendError::Other`] with the detail spelled out.
    pub fn into_backend_error(self, scanner: &ScannerId) -> BackendError {
        match self {
            Self::Unreachable { .. } => BackendError::NotReachable {
                scanner: scanner.clone(),
                detail: self.to_string(),
            },
            Self::Refused { .. } => BackendError::Unsupported {
                backend: crate::ID,
                operation: "scan-to-PC registration",
            },
            Self::Registration(_) | Self::Unexpected { .. } => {
                BackendError::Other(format!("scanner {scanner}: {self}"))
            }
        }
    }
}

impl From<RegisterError> for RegistrarError {
    fn from(error: RegisterError) -> Self {
        Self::Registration(error)
    }
}

/// One device's registrations: what is registered, and what keeps them alive.
///
/// Owns a task per device rather than a global one because the lease is per device and
/// so is the failure: an unplugged printer must not delay the refresh of another one.
/// (The *listener* is the opposite — one socket for every device, issue 5.9 — because
/// UDP/54925 is a fixed port and not a per-device resource.)
#[derive(Debug)]
pub struct Registrar {
    session: Session,
    refresh: Option<JoinHandle<()>>,
}

impl Registrar {
    /// A registrar for one device, advertising the address that routes to it.
    ///
    /// The user name is a parameter rather than [`panel_name`] taken directly, because
    /// what appears on the panel is a decision the daemon may want to make once for every
    /// device rather than a fact this module discovers per registrar.
    pub fn new(device: Ipv4Addr, user: UserName, transport: Arc<dyn SnmpTransport>) -> Self {
        Self {
            session: Session {
                exchange: Exchange {
                    device,
                    community: snmp::DEFAULT_COMMUNITY.into(),
                    transport,
                    request_ids: Arc::new(AtomicI32::new(snmp::FIRST_REQUEST_ID)),
                },
                user,
                host: Arc::new(RoutedHost),
                duration: DEFAULT_DURATION,
                functions: BTreeSet::new(),
            },
            refresh: None,
        }
    }

    /// A lease other than the vendor's 360 s. Tests want a short one; nothing else does.
    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.session.duration = duration;
        self
    }

    /// A community other than `internal`, for a device configured away from the default.
    pub fn with_community(mut self, community: impl Into<Arc<str>>) -> Self {
        self.session.exchange.community = community.into();
        self
    }

    /// Advertise a fixed address instead of asking the routing table.
    pub fn with_host(self, host: Ipv4Addr) -> Self {
        self.with_host_address(Arc::new(FixedHost(host)))
    }

    /// Advertise whatever this resolver says, re-asked before every refresh.
    pub fn with_host_address(mut self, host: Arc<dyn HostAddress>) -> Self {
        self.session.host = host;
        self
    }

    pub fn device(&self) -> Ipv4Addr {
        self.session.exchange.device
    }

    /// The functions currently being kept alive — empty when nothing is running.
    pub fn functions(&self) -> &BTreeSet<Function> {
        &self.session.functions
    }

    pub fn is_running(&self) -> bool {
        self.refresh.is_some()
    }

    /// Register every requested function once, then keep them alive until [`stop`](Self::stop).
    ///
    /// The first round happens **here**, awaited, rather than inside the spawned task, so
    /// that a caller pairing a scanner learns what the device said instead of learning
    /// nothing and finding out from the panel. A device that refuses leaves no task
    /// running: there is nothing to renew, and retrying a `noSuchName` every three
    /// minutes forever would be noise about a model that is never going to say yes.
    ///
    /// Calling it again replaces what is registered, which is how a button mapping
    /// changes: functions that are no longer requested simply stop being refreshed and
    /// leave the panel within one lease.
    pub async fn start(
        &mut self,
        functions: impl IntoIterator<Item = Function>,
    ) -> Result<(), RegistrarError> {
        self.stop();

        let functions: BTreeSet<Function> = functions.into_iter().collect();
        if functions.is_empty() {
            return Ok(());
        }
        self.session.functions = functions;

        match self.first_round().await {
            Ok(host) => {
                self.refresh = Some(tokio::spawn(
                    Refresh {
                        session: self.session.clone(),
                        host,
                    }
                    .run(),
                ));
                Ok(())
            }
            // Nothing is registered and nothing is running, so nothing is being kept
            // alive: saying otherwise would have `functions()` describe a panel that
            // does not have those entries on it.
            Err(error) => {
                self.session.functions.clear();
                Err(error)
            }
        }
    }

    /// The registration round that [`start`](Self::start) awaits, first failure wins.
    ///
    /// Unlike a refresh round this one gives up at the first refusal: the caller is
    /// pairing, it is about to be told, and asking the same device for three more
    /// functions it has just said it does not understand buys nothing.
    async fn first_round(&self) -> Result<Ipv4Addr, RegistrarError> {
        let host = self.session.host().await?;
        for function in &self.session.functions {
            self.session.register(host, *function).await?;
            info!(
                device = %self.device(),
                %function,
                %host,
                user = %self.session.user,
                lease_s = self.session.duration.as_secs(),
                "registered with the device",
            );
        }
        Ok(host)
    }

    /// Stop refreshing. The panel entries lapse within one `DURATION`; nothing is sent.
    pub fn stop(&mut self) {
        if let Some(refresh) = self.refresh.take() {
            refresh.abort();
            debug!(
                device = %self.device(),
                lease_s = self.session.duration.as_secs(),
                "stopped refreshing; the panel entries lapse within one lease",
            );
        }
        self.session.functions.clear();
    }
}

/// Dropping a registrar is stopping it: the task holds a transport and would otherwise
/// keep a device registered to a host that has forgotten about it.
impl Drop for Registrar {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Everything one registration round needs: whom to ask, as whom, from where, for how
/// long, and for which functions.
///
/// Cloned into the refresh task rather than shared, so that dropping the [`Registrar`]
/// cannot leave the task borrowing anything.
#[derive(Debug, Clone)]
struct Session {
    exchange: Exchange,
    user: UserName,
    host: Arc<dyn HostAddress>,
    duration: Duration,
    functions: BTreeSet<Function>,
}

impl Session {
    /// The address to put in `HOST=`, asked *now*.
    async fn host(&self) -> Result<Ipv4Addr, RegistrarError> {
        let device = self.exchange.device;
        self.host
            .towards(device)
            .await
            .map_err(|source| RegistrarError::Unreachable { device, source })
    }

    async fn register(&self, host: Ipv4Addr, function: Function) -> Result<(), RegistrarError> {
        self.exchange
            .set(&Registration {
                user: self.user.clone(),
                host,
                port: LISTENER_PORT,
                function,
                duration: self.duration,
            })
            .await
    }
}

/// The refresh task: renew, wait, renew.
#[derive(Debug)]
struct Refresh {
    session: Session,
    /// The address the last round advertised, so that a change can be said out loud.
    host: Ipv4Addr,
}

impl Refresh {
    /// Every await point here is a cancellation point, and being cancelled at any of them
    /// leaves nothing to clean up — the only state a round creates is on the device, and
    /// it is a lease that expires by itself.
    async fn run(mut self) {
        let mut delay = refresh_delay(self.session.duration);
        loop {
            sleep(delay).await;

            delay = if self.round().await {
                retry_delay(self.session.duration)
            } else {
                refresh_delay(self.session.duration)
            };
        }
    }

    /// One round: re-derive the source address, then renew every function. Returns
    /// whether anything failed.
    ///
    /// Best-effort across functions, unlike the first round: three entries on the panel
    /// and one that fell off is a better outcome than four that did, and the one that
    /// failed gets another attempt at the retry delay.
    async fn round(&mut self) -> bool {
        let device = self.session.exchange.device;

        let host = match self.session.host().await {
            Ok(host) => host,
            Err(error) => {
                warn!(
                    %device,
                    %error,
                    "no route to this device; the panel entries will lapse if this does not \
                     recover",
                );
                return true;
            }
        };
        if host != self.host {
            // The interesting log line of the whole module: it is what a "the printer
            // stopped reacting after I connected to the VPN" report is diagnosed with.
            info!(
                %device,
                was = %self.host,
                now = %host,
                "the route to this device changed; re-registering from the new address",
            );
            self.host = host;
        }

        let mut failed = false;
        for function in &self.session.functions {
            match self.session.register(host, *function).await {
                Ok(()) => debug!(%device, %function, %host, "lease renewed"),
                Err(error) => {
                    failed = true;
                    warn!(
                        %device,
                        %function,
                        %error,
                        "could not renew the lease; the panel entry will lapse if this does \
                         not recover",
                    );
                }
            }
        }
        failed
    }
}

/// One device, one community, one request-id counter: everything a `SetRequest` needs
/// that is not the registration itself.
#[derive(Debug, Clone)]
struct Exchange {
    device: Ipv4Addr,
    community: Arc<str>,
    transport: Arc<dyn SnmpTransport>,
    /// Shared with the refresh task so ids keep rising across the handover, the way the
    /// vendor's single counter does. Nothing depends on the value — the id has to be
    /// echoed back, not predicted — but two requests in flight with the same id would
    /// make the echo useless.
    request_ids: Arc<AtomicI32>,
}

impl Exchange {
    async fn set(&self, registration: &Registration) -> Result<(), RegistrarError> {
        let request_id = self.request_ids.fetch_add(1, Ordering::Relaxed);
        let request = registration.set_request(&self.community, request_id)?;

        let response = send(&*self.transport, self.device, &request).await?;
        accepted(
            self.device,
            request_id,
            Some(registration.function),
            &response,
        )
    }
}

/// Request ids for the one-off exchanges no [`Registrar`] owns.
///
/// Process-wide, for the same reason a registrar's counter is registrar-wide: nothing
/// predicts the value, but two outstanding requests sharing one makes the echo — the only
/// thing that says which answer belongs to which question — meaningless.
static PROBE_REQUEST_IDS: AtomicI32 = AtomicI32::new(snmp::FIRST_REQUEST_ID);

/// Ask a device whether it knows the scan-key OID at all, changing nothing on it.
///
/// A `GetRequest`, not a `SetRequest`: this is the question pairing asks, and pairing must
/// not put an entry on the panel that nothing is yet listening behind. `noSuchName` — a
/// [`RegistrarError::Refused`] — is the documented degraded path of
/// [`brother-skeyless-backend.md`] §4, an older or newer generation that documents TCP
/// 5566 or 54921 instead, and it is a statement about the model that is worth recording.
/// A timeout is [`RegistrarError::Unreachable`] and is worth recording nothing at all.
///
/// [`brother-skeyless-backend.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/brother-skeyless-backend.md
pub async fn probe_scan_to_pc(
    transport: &dyn SnmpTransport,
    device: Ipv4Addr,
    community: &str,
) -> Result<(), RegistrarError> {
    let request_id = PROBE_REQUEST_IDS.fetch_add(1, Ordering::Relaxed);
    let response = send(transport, device, &probe_request(community, request_id)).await?;
    accepted(device, request_id, None, &response)
}

async fn send(
    transport: &dyn SnmpTransport,
    device: Ipv4Addr,
    request: &Message,
) -> Result<Message, RegistrarError> {
    transport
        .exchange(SocketAddrV4::new(device, snmp::SNMP_PORT), request)
        .await
        .map_err(|source| RegistrarError::Unreachable { device, source })
}

/// Is this the device saying yes to *that* request?
fn accepted(
    device: Ipv4Addr,
    request_id: i32,
    function: Option<Function>,
    response: &Message,
) -> Result<(), RegistrarError> {
    let unexpected = |reason: String| RegistrarError::Unexpected { device, reason };

    if response.pdu.kind != PduKind::Response {
        return Err(unexpected(format!(
            "a {:?}, not a Response",
            response.pdu.kind
        )));
    }
    if response.pdu.request_id != request_id {
        return Err(unexpected(format!(
            "request-id {} while {request_id} was outstanding",
            response.pdu.request_id
        )));
    }
    if !response.pdu.error_status.is_ok() {
        return Err(RegistrarError::Refused {
            device,
            function,
            status: response.pdu.error_status,
        });
    }
    Ok(())
}

/// The real transport: one unconnected-then-connected UDP socket per exchange.
///
/// A socket per exchange rather than one kept open, because there are four of them every
/// three minutes and a socket held open across a suspend is a socket bound to an address
/// the machine may no longer have. `connect` also gets the kernel to filter the replies
/// by peer for us, which is half of "the answer to *that* request".
#[derive(Debug, Clone, Copy)]
pub struct UdpSnmp {
    timeout: Duration,
}

impl Default for UdpSnmp {
    fn default() -> Self {
        Self::new(RESPONSE_TIMEOUT)
    }
}

impl UdpSnmp {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }

    async fn round_trip(
        &self,
        device: SocketAddrV4,
        request: &Message,
    ) -> Result<Message, TransportError> {
        let bytes = request.encode().map_err(TransportError::Encode)?;

        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
            .await
            .map_err(|error| TransportError::Io(error.to_string()))?;
        socket
            .connect(device)
            .await
            .map_err(|error| TransportError::Io(error.to_string()))?;
        socket
            .send(&bytes)
            .await
            .map_err(|error| TransportError::Io(error.to_string()))?;

        let mut buffer = vec![0u8; RECV_BUFFER];
        loop {
            let read = socket
                .recv(&mut buffer)
                .await
                .map_err(|error| TransportError::Io(error.to_string()))?;
            let message = Message::decode(&buffer[..read]).map_err(TransportError::Malformed)?;
            // The kernel filters by peer, not by request: a device that is also being
            // polled by something else on this host can put an unrelated Response in
            // front of ours, and dropping it is cheaper than failing the registration.
            if message.pdu.request_id == request.pdu.request_id {
                return Ok(message);
            }
            debug!(
                device = %device,
                request_id = message.pdu.request_id,
                "ignoring an SNMP answer to a request that is not ours",
            );
        }
    }
}

#[async_trait]
impl SnmpTransport for UdpSnmp {
    async fn exchange(
        &self,
        device: SocketAddrV4,
        request: &Message,
    ) -> Result<Message, TransportError> {
        match timeout(self.timeout, self.round_trip(device, request)).await {
            Ok(result) => result,
            Err(_) => Err(TransportError::Timeout(self.timeout)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tokio::time::Instant;

    use super::*;
    use crate::skey::register::REGISTRATION_OID;
    use crate::skey::snmp::{Pdu, Value, VarBind, Version};

    /// What the fake device does with each request it is given.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Answer {
        Ok,
        Refuse(ErrorStatus),
        Silence,
        /// A well-formed Response to somebody else's request.
        WrongRequestId,
    }

    /// One recorded exchange: when it happened, where it went, and what was in it.
    #[derive(Debug, Clone)]
    struct Sent {
        at: Duration,
        target: SocketAddrV4,
        request: Message,
    }

    impl Sent {
        fn registration(&self) -> Registration {
            let value = self.request.pdu.varbinds[0]
                .value
                .as_str()
                .expect("a registration is an octet string");
            Registration::parse(value).expect("what we sent parses back")
        }
    }

    #[derive(Debug)]
    struct FakeDevice {
        started: Instant,
        sent: Mutex<Vec<Sent>>,
        /// Answers, in order; the last one repeats forever.
        answers: Mutex<Vec<Answer>>,
    }

    impl FakeDevice {
        fn answering(answers: impl IntoIterator<Item = Answer>) -> Arc<Self> {
            Arc::new(Self {
                started: Instant::now(),
                sent: Mutex::new(Vec::new()),
                answers: Mutex::new(answers.into_iter().collect()),
            })
        }

        fn ok() -> Arc<Self> {
            Self::answering([Answer::Ok])
        }

        fn sent(&self) -> Vec<Sent> {
            self.sent.lock().unwrap().clone()
        }

        fn count(&self) -> usize {
            self.sent.lock().unwrap().len()
        }

        fn next_answer(&self) -> Answer {
            let mut answers = self.answers.lock().unwrap();
            if answers.len() > 1 {
                answers.remove(0)
            } else {
                answers[0]
            }
        }
    }

    #[async_trait]
    impl SnmpTransport for FakeDevice {
        async fn exchange(
            &self,
            device: SocketAddrV4,
            request: &Message,
        ) -> Result<Message, TransportError> {
            self.sent.lock().unwrap().push(Sent {
                at: Instant::now().duration_since(self.started),
                target: device,
                request: request.clone(),
            });

            let (status, request_id) = match self.next_answer() {
                Answer::Ok => (ErrorStatus::NoError, request.pdu.request_id),
                Answer::Refuse(status) => (status, request.pdu.request_id),
                Answer::WrongRequestId => (ErrorStatus::NoError, request.pdu.request_id + 1000),
                Answer::Silence => {
                    // What the real transport does with a printer that is unplugged: it
                    // waits out the timeout and reports it.
                    sleep(RESPONSE_TIMEOUT).await;
                    return Err(TransportError::Timeout(RESPONSE_TIMEOUT));
                }
            };
            Ok(Message {
                version: Version::V1,
                community: request.community.clone(),
                pdu: Pdu {
                    kind: PduKind::Response,
                    request_id,
                    error_status: status,
                    error_index: 0,
                    varbinds: vec![VarBind::new(
                        request.pdu.varbinds[0].oid.clone(),
                        Value::OctetString(b"TRUE".to_vec()),
                    )],
                },
            })
        }
    }

    const DEVICE: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 3);
    const HOST: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 20);
    /// The address a VPN or a docker bridge would offer instead — routable to nothing
    /// the printer can see.
    const OTHER_HOST: Ipv4Addr = Ipv4Addr::new(10, 8, 0, 2);
    /// Short enough to be readable in an assertion, long enough that half of it is not
    /// the same number as a retry.
    const LEASE: Duration = Duration::from_secs(360);

    fn registrar(transport: Arc<dyn SnmpTransport>) -> Registrar {
        Registrar::new(DEVICE, UserName::new("desktop").unwrap(), transport)
            .with_duration(LEASE)
            .with_host(HOST)
    }

    /// A routing table that changes under a running lease — a VPN coming up, a laptop
    /// moving between networks — plus the failure of having no route at all.
    #[derive(Debug)]
    struct Routing {
        answers: Mutex<Vec<Result<Ipv4Addr, String>>>,
        asked: Mutex<Vec<Ipv4Addr>>,
    }

    impl Routing {
        fn answering(answers: impl IntoIterator<Item = Result<Ipv4Addr, String>>) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(answers.into_iter().collect()),
                asked: Mutex::new(Vec::new()),
            })
        }

        fn asked(&self) -> usize {
            self.asked.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl HostAddress for Routing {
        async fn towards(&self, device: Ipv4Addr) -> Result<Ipv4Addr, TransportError> {
            self.asked.lock().unwrap().push(device);
            let mut answers = self.answers.lock().unwrap();
            let answer = if answers.len() > 1 {
                answers.remove(0)
            } else {
                answers[0].clone()
            };
            answer.map_err(TransportError::Io)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn starting_sends_one_set_per_requested_function() {
        let device = FakeDevice::ok();
        let mut registrar = registrar(device.clone());
        registrar
            .start([Function::Image, Function::File])
            .await
            .unwrap();

        let sent = device.sent();
        assert_eq!(sent.len(), 2, "one SetRequest per function, no more");

        // In button order, because that is the order a panel and a `Button1` index agree
        // on — File is 0.
        let functions: Vec<Function> = sent.iter().map(|s| s.registration().function).collect();
        assert_eq!(functions, [Function::File, Function::Image]);

        for exchange in &sent {
            assert_eq!(exchange.target, SocketAddrV4::new(DEVICE, 161));
            assert_eq!(exchange.request.pdu.kind, PduKind::SetRequest);
            assert_eq!(exchange.request.community, b"internal");
            assert_eq!(
                exchange.request.pdu.varbinds[0].oid.to_string(),
                REGISTRATION_OID
            );

            let registration = exchange.registration();
            assert_eq!(
                registration.host, HOST,
                "HOST= is where the press comes back"
            );
            assert_eq!(registration.port, 54925);
            assert_eq!(registration.duration, LEASE);
            assert_eq!(registration.user.as_str(), "desktop");
        }

        // Two requests in flight must never share an id: the echo is the only thing that
        // says which answer belongs to which registration.
        assert_eq!(sent[0].request.pdu.request_id, snmp::FIRST_REQUEST_ID);
        assert_eq!(sent[1].request.pdu.request_id, snmp::FIRST_REQUEST_ID + 1);
        assert!(registrar.is_running());
        assert_eq!(
            registrar.functions(),
            &BTreeSet::from([Function::File, Function::Image])
        );
    }

    /// The whole point of the module: the lease is renewed at half of it, forever.
    #[tokio::test(start_paused = true)]
    async fn the_lease_is_renewed_at_half_its_duration() {
        let device = FakeDevice::ok();
        let mut registrar = registrar(device.clone());
        registrar.start([Function::Image]).await.unwrap();

        // Three full leases: a device left registered for 2 × DURATION is the acceptance
        // criterion, and only a *repeating* renewal survives it. The extra second is so
        // the assertion is not about which of two tasks the runtime wakes first at a
        // refresh instant that falls exactly on the sleep's own deadline.
        sleep(LEASE * 3 + Duration::from_secs(1)).await;

        let at: Vec<u64> = device.sent().iter().map(|s| s.at.as_secs()).collect();
        assert_eq!(at, [0, 180, 360, 540, 720, 900, 1080]);

        // Every renewal is the same registration, not a degraded one.
        for exchange in device.sent() {
            assert_eq!(exchange.registration().function, Function::Image);
            assert_eq!(exchange.registration().duration, LEASE);
        }
    }

    /// Stopping is not sending anything: the entry lapses on its own.
    #[tokio::test(start_paused = true)]
    async fn stopping_stops_refreshing_and_sends_no_teardown() {
        let device = FakeDevice::ok();
        let mut registrar = registrar(device.clone());
        registrar.start(Function::ALL).await.unwrap();
        sleep(LEASE).await;
        let before = device.count();
        assert!(before > 4, "it was refreshing before it was stopped");

        registrar.stop();
        sleep(LEASE * 3).await;

        assert_eq!(
            device.count(),
            before,
            "stop must send nothing and renew nothing"
        );
        assert!(!registrar.is_running());
        assert!(registrar.functions().is_empty());
        assert!(
            device
                .sent()
                .iter()
                .all(|s| s.request.pdu.kind == PduKind::SetRequest),
            "the only thing this module ever sends is a registration",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_registrar_stops_the_refresh_task() {
        let device = FakeDevice::ok();
        let mut registrar = registrar(device.clone());
        registrar.start([Function::Image]).await.unwrap();
        drop(registrar);

        let after_drop = device.count();
        sleep(LEASE * 3).await;
        assert_eq!(device.count(), after_drop);
    }

    /// Starting again replaces what is registered — how a button mapping changes.
    #[tokio::test(start_paused = true)]
    async fn starting_again_replaces_the_registered_functions() {
        let device = FakeDevice::ok();
        let mut registrar = registrar(device.clone());
        registrar
            .start([Function::Image, Function::Email])
            .await
            .unwrap();
        registrar.start([Function::Ocr]).await.unwrap();

        let before = device.count();
        sleep(LEASE + Duration::from_secs(1)).await;
        let renewed: Vec<Function> = device.sent()[before..]
            .iter()
            .map(|s| s.registration().function)
            .collect();
        assert_eq!(
            renewed,
            [Function::Ocr, Function::Ocr],
            "only the functions of the last start are kept alive",
        );
    }

    /// A device that refuses the OID is the supported degraded path, and it has to be
    /// distinguishable from one that is switched off — 5.8's whole error-mapping item
    /// rests on these being two variants and not one.
    #[tokio::test(start_paused = true)]
    async fn a_refusal_is_reported_as_a_refusal_and_leaves_nothing_running() {
        let device = FakeDevice::answering([Answer::Refuse(ErrorStatus::NoSuchName)]);
        let mut registrar = registrar(device.clone());
        let error = registrar.start([Function::Image]).await.unwrap_err();

        assert_eq!(
            error,
            RegistrarError::Refused {
                device: DEVICE,
                function: Some(Function::Image),
                status: ErrorStatus::NoSuchName,
            }
        );
        assert!(!registrar.is_running(), "nothing to renew, so nothing runs");

        sleep(LEASE * 3).await;
        assert_eq!(
            device.count(),
            1,
            "a model that will never say yes is not asked every three minutes",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_device_is_unreachable_rather_than_refusing() {
        let device = FakeDevice::answering([Answer::Silence]);
        let mut registrar = registrar(device.clone());
        let error = registrar.start([Function::Image]).await.unwrap_err();

        assert_eq!(
            error,
            RegistrarError::Unreachable {
                device: DEVICE,
                source: TransportError::Timeout(RESPONSE_TIMEOUT),
            }
        );
        assert!(!registrar.is_running());
    }

    /// Somebody else's answer is not our answer.
    #[tokio::test(start_paused = true)]
    async fn an_answer_to_another_request_is_not_taken_as_success() {
        let device = FakeDevice::answering([Answer::WrongRequestId]);
        let mut registrar = registrar(device.clone());
        let error = registrar.start([Function::Image]).await.unwrap_err();

        assert!(
            matches!(error, RegistrarError::Unexpected { .. }),
            "{error}"
        );
    }

    /// The printer is unplugged mid-lease: retry sooner than the lease, without spinning,
    /// and be registered again within one lease of it coming back.
    #[tokio::test(start_paused = true)]
    async fn a_failed_renewal_retries_without_spinning_and_recovers() {
        // Registered, then silent for a while, then answering again.
        let mut answers = vec![Answer::Ok];
        answers.extend(std::iter::repeat_n(Answer::Silence, 8));
        answers.push(Answer::Ok);
        let device = FakeDevice::answering(answers);

        let mut registrar = registrar(device.clone());
        registrar.start([Function::Image]).await.unwrap();

        sleep(LEASE * 2).await;
        let attempts = device.count();
        assert!(
            (2..=24).contains(&attempts),
            "over two leases a dead printer was asked {attempts} times: \
             that is either a spin or a give-up",
        );

        // The last attempt succeeded, and it happened within a lease of the device
        // answering again rather than at the next scheduled refresh.
        let sent = device.sent();
        let recovered_at = sent.last().unwrap().at;
        let last_failure_at = sent[sent.len() - 2].at;
        assert!(
            recovered_at - last_failure_at <= LEASE,
            "recovery waited {:?}, longer than the lease it was protecting",
            recovered_at - last_failure_at,
        );
    }

    #[test]
    fn a_lease_is_refreshed_at_half_and_retried_between_five_and_sixty_seconds() {
        assert_eq!(refresh_delay(DEFAULT_DURATION), Duration::from_secs(180));
        assert_eq!(retry_delay(DEFAULT_DURATION), Duration::from_secs(30));
        // A very short lease must not turn the retry into a spin, and a very long one
        // must not make it useless.
        assert_eq!(retry_delay(Duration::from_secs(12)), Duration::from_secs(5));
        assert_eq!(
            retry_delay(Duration::from_secs(24 * 3600)),
            Duration::from_secs(60)
        );
    }

    /// A name the device would reject never reaches the network — the failure is in the
    /// registration string, and saying "the printer did not answer" would send the user
    /// looking at the network.
    #[tokio::test(start_paused = true)]
    async fn an_impossible_lease_fails_before_the_socket() {
        let device = FakeDevice::ok();
        let mut registrar = registrar(device.clone()).with_duration(Duration::ZERO);
        let error = registrar.start([Function::Image]).await.unwrap_err();

        assert!(
            matches!(
                error,
                RegistrarError::Registration(RegisterError::Duration(0))
            ),
            "{error}"
        );
        assert_eq!(device.count(), 0, "nothing was sent");
    }

    #[tokio::test(start_paused = true)]
    async fn registering_nothing_starts_no_task_and_sends_nothing() {
        let device = FakeDevice::ok();
        let mut registrar = registrar(device.clone());
        registrar.start([]).await.unwrap();

        assert_eq!(device.count(), 0);
        assert!(!registrar.is_running());
    }

    // ------------------------------------------------------- where the press comes back

    /// The address is asked for again on every round, so a machine that changes network
    /// re-registers from the address that can actually be reached — within one lease, and
    /// without anybody restarting anything.
    #[tokio::test(start_paused = true)]
    async fn the_source_address_is_re_derived_before_every_refresh() {
        // Registered from the LAN address; then the route is gone for one round — a
        // resume, a VPN coming up — and then it is a different address.
        let routing = Routing::answering([
            Ok(HOST),
            Err("network is unreachable".to_owned()),
            Ok(OTHER_HOST),
        ]);
        let device = FakeDevice::ok();
        let mut registrar =
            Registrar::new(DEVICE, UserName::new("desktop").unwrap(), device.clone())
                .with_duration(LEASE)
                .with_host_address(routing.clone());

        registrar.start([Function::Image]).await.unwrap();
        assert_eq!(device.sent()[0].registration().host, HOST);

        sleep(LEASE + Duration::from_secs(1)).await;

        let sent = device.sent();
        let hosts: Vec<Ipv4Addr> = sent.iter().map(|s| s.registration().host).collect();
        assert_eq!(
            hosts,
            [HOST, OTHER_HOST],
            "a round with no route sends nothing, and the next one advertises the new address",
        );
        assert!(
            routing.asked() > sent.len(),
            "the routing table was asked once per round, including the round that failed",
        );
        assert!(
            sent[1].at <= LEASE,
            "the new address reached the device after {:?}, more than the lease it protects",
            sent[1].at,
        );
    }

    /// A device with no route to it never gets a registration built for it: the failure
    /// is "unreachable", not a registration carrying whatever address was left over.
    #[tokio::test(start_paused = true)]
    async fn a_device_with_no_route_is_unreachable_and_nothing_is_sent() {
        let routing = Routing::answering([Err("network is unreachable".to_owned())]);
        let device = FakeDevice::ok();
        let mut registrar =
            Registrar::new(DEVICE, UserName::new("desktop").unwrap(), device.clone())
                .with_duration(LEASE)
                .with_host_address(routing);

        let error = registrar.start([Function::Image]).await.unwrap_err();
        assert_eq!(
            error,
            RegistrarError::Unreachable {
                device: DEVICE,
                source: TransportError::Io("network is unreachable".to_owned()),
            }
        );
        assert_eq!(device.count(), 0, "nothing was sent");
        assert!(!registrar.is_running());
        assert!(!error.is_refusal(), "no route is not a refusal");
    }

    // ---------------------------------------------------------- the name on the panel

    /// Every case here is an ordinary hostname that the device's 15-alphanumeric rule
    /// would reject, or accept as something the user would not recognise.
    #[test]
    fn the_panel_name_is_the_machines_name_reduced_to_what_the_device_accepts() {
        for (hostname, expected) in [
            ("desktop", "desktop"),
            // The domain goes before the truncation, or every machine in the office
            // appears on the panel as the same fifteen characters.
            ("desktop.office.example.com", "desktop"),
            ("workstation-04.lan", "workstation04"),
            ("a-very-long-hostname-indeed", "averylonghostna"),
            ("café", "caf"),
            // Nothing usable left: a name on the panel is better than no panel entry.
            ("", FALLBACK_PANEL_NAME),
            ("---", FALLBACK_PANEL_NAME),
            (".", FALLBACK_PANEL_NAME),
        ] {
            assert_eq!(
                panel_name_from(hostname).as_str(),
                expected,
                "hostname {hostname:?}",
            );
        }
    }

    /// The real one, on whatever machine the suite runs on: registrable, and the same
    /// answer every time — which is also what makes "logged once" true.
    #[test]
    fn this_machines_panel_name_is_registrable_and_stable() {
        let name = panel_name();
        assert!(UserName::new(name.as_str()).is_ok(), "{name}");
        assert_eq!(name, panel_name());
    }

    // ------------------------------------------------------------ what the caller hears

    /// Unreachable and refused are two different answers to the user: one says "switch
    /// the printer on", the other says "this model cannot do this at all".
    #[test]
    fn a_silent_device_and_a_refusing_one_map_to_different_backend_errors() {
        let scanner = ScannerId::from_backend("brother", "192.168.1.3").unwrap();

        let unreachable = RegistrarError::Unreachable {
            device: DEVICE,
            source: TransportError::Timeout(RESPONSE_TIMEOUT),
        }
        .into_backend_error(&scanner);
        assert!(
            matches!(unreachable, BackendError::NotReachable { scanner: ref s, .. } if *s == scanner),
            "{unreachable:?}",
        );

        let refused = RegistrarError::Refused {
            device: DEVICE,
            function: Some(Function::Image),
            status: ErrorStatus::NoSuchName,
        }
        .into_backend_error(&scanner);
        assert_eq!(
            refused,
            BackendError::Unsupported {
                backend: crate::ID,
                operation: "scan-to-PC registration",
            },
            "a refusal is NotSupported, not NotReachable with a longer message",
        );

        // The two local failures are neither, and say so rather than blaming the device.
        for local in [
            RegistrarError::Registration(RegisterError::Duration(0)),
            RegistrarError::Unexpected {
                device: DEVICE,
                reason: "a GetRequest".to_owned(),
            },
        ] {
            assert!(
                matches!(
                    local.clone().into_backend_error(&scanner),
                    BackendError::Other(_)
                ),
                "{local}",
            );
        }
    }

    // --------------------------------------------------------------------- the probe

    /// Pairing asks the question with a read, so that a model that says no never gets an
    /// entry on its panel that nothing is listening behind.
    #[tokio::test(start_paused = true)]
    async fn probing_asks_a_read_only_question() {
        let device = FakeDevice::ok();
        probe_scan_to_pc(&*device.clone(), DEVICE, snmp::DEFAULT_COMMUNITY)
            .await
            .unwrap();

        let sent = device.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].request.pdu.kind,
            PduKind::GetRequest,
            "the probe must not write to the device",
        );
        assert_eq!(sent[0].target, SocketAddrV4::new(DEVICE, 161));
        assert_eq!(
            sent[0].request.pdu.varbinds[0].oid.to_string(),
            REGISTRATION_OID
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_probe_tells_a_model_that_will_not_apart_from_one_that_is_off() {
        let refusing = FakeDevice::answering([Answer::Refuse(ErrorStatus::NoSuchName)]);
        let error = probe_scan_to_pc(&*refusing, DEVICE, snmp::DEFAULT_COMMUNITY)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            RegistrarError::Refused {
                device: DEVICE,
                function: None,
                status: ErrorStatus::NoSuchName,
            }
        );
        assert!(error.is_refusal());
        assert!(
            error.to_string().contains("does not appear to support"),
            "{error}",
        );

        let silent = FakeDevice::answering([Answer::Silence]);
        let error = probe_scan_to_pc(&*silent, DEVICE, snmp::DEFAULT_COMMUNITY)
            .await
            .unwrap_err();
        assert!(
            matches!(error, RegistrarError::Unreachable { .. }),
            "{error}"
        );
        assert!(
            !error.is_refusal(),
            "a printer that is off says nothing about the model",
        );
    }

    // ------------------------------------------------------------------- cancellation

    /// Dropped in the middle of an exchange — the cancellation the trait's contract is
    /// about. Nothing is owed to the device, so nothing is left behind: the lease it
    /// still holds expires by itself, and a fresh registrar starts clean.
    #[tokio::test(start_paused = true)]
    async fn cancelling_the_refresh_mid_exchange_leaves_nothing_behind() {
        let device = FakeDevice::answering([Answer::Ok, Answer::Silence]);
        let mut running = registrar(device.clone());
        running.start([Function::Image]).await.unwrap();

        // Past the first renewal and into the two-second wait for an answer it will
        // never get: the task is parked inside the transport when it is cancelled.
        sleep(refresh_delay(LEASE) + Duration::from_secs(1)).await;
        assert_eq!(device.count(), 2, "a renewal is in flight");

        drop(running);
        sleep(LEASE * 3).await;
        assert_eq!(
            device.count(),
            2,
            "a cancelled refresh sends nothing more, in flight or not",
        );

        // And the module is not left in a state a second registrar trips over — a fresh
        // device, because this one is still playing dead.
        let recovered = FakeDevice::ok();
        let mut again = registrar(recovered.clone());
        again.start([Function::Image]).await.unwrap();
        assert!(again.is_running());
        assert_eq!(recovered.count(), 1);
    }

    /// Stopping twice, and stopping something that never started, are both no-ops —
    /// `Drop` runs `stop` again after every explicit call.
    #[tokio::test(start_paused = true)]
    async fn stopping_is_idempotent() {
        let device = FakeDevice::ok();
        let mut registrar = registrar(device.clone());
        registrar.stop();
        registrar.start([Function::Image]).await.unwrap();
        registrar.stop();
        registrar.stop();

        assert!(!registrar.is_running());
        sleep(LEASE * 2).await;
        assert_eq!(device.count(), 1);
    }
}
