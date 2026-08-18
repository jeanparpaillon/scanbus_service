//! One socket for every Brother, and which scanner each datagram belongs to.
//!
//! [`skey::event`](crate::skey::event) turns bytes into a [`KeyPress`]; this module is
//! what those bytes arrive on.
//!
//! # One socket, not one per scanner
//!
//! UDP/54925 is fixed and well known — Brother documents it as *the* firewall port to open
//! for network scanning — so it is not a per-scanner resource. Exactly one process on the
//! machine can bind it, and every registered device sends to it. The listener is therefore
//! a **process-wide singleton owned by the backend**: [`BrotherBackend`](crate::BrotherBackend)
//! holds one [`Listener`] behind an `Arc` that every clone of the backend shares, it is
//! bound on the first subscriber and closed on the last, and each `start_listening()`
//! caller gets its own [`Subscription`] stream.
//!
//! That is [`MobileBackend`]'s `ListenerBinding` (9.4) with one difference: the mobile
//! listener binds once at construction because its port is negotiable and is advertised
//! during pairing, while this port is a constant nobody negotiates. Holding it from
//! process start would deny it to `brscan-skey` for the entire life of a daemon that may
//! never have a Brother device paired, so the bind is deferred to the first subscriber.
//!
//! # Demultiplexing by source address
//!
//! The datagram *does* carry the device's own address in `CLIENT=`, and it is deliberately
//! not what selects the scanner: the registration this press answers ([`registrar`]) was
//! addressed to a device we chose **by address**, so the address is the identity already
//! trusted. `CLIENT=` is compared against it and a disagreement is logged, because a
//! device that reports an address it is not sending from is the diagnosis for a NAT or a
//! second interface in the path — but it never changes which stream the press lands on.
//!
//! A datagram from an address no subscribed scanner matches is logged at debug and
//! dropped. On a shared network that is somebody else's printer talking to a host that
//! happens to be us by mistake, or a stale registration from a previous boot, and neither
//! is an error worth a warning on a socket anyone can send to.
//!
//! # `EADDRINUSE` is `brscan-skey`, and is not worked around
//!
//! On a machine that has ever done Brother scanning there is one overwhelmingly likely
//! owner of this port, and reporting a generic bind failure sends the user to look at
//! firewalls. [`BindError::Taken`] is therefore its own variant, and
//! [`BrotherBackend::start_listening`](crate::BrotherBackend) words it by name.
//!
//! `SO_REUSEPORT` would let both processes bind and is the wrong answer: the kernel then
//! hands each datagram to *one* of them by hash, so half the panel presses would go to the
//! vendor daemon and the bug would reproduce on every second press. Refusing is the honest
//! outcome — the scanner stays discovered and pull-scannable, and only walk-up is lost.
//!
//! # Nothing is sent back
//!
//! The device expects no acknowledgement. In the vendor daemon, `udp_agent` (`0x407b15`)
//! calls only `set_sync_event`, `get_last_recv_ip_address`, `sprintf` and `strcpy` after
//! `check_udp_data`; there is no path from a received datagram to `udp_sent`, whose only
//! caller for this socket is the daemon posting `Refresh Device List` to *itself*. So this
//! socket writes nothing at all. If the packet capture 5.7 still wants ever shows a device
//! retransmitting a press it considers unacknowledged, this is the module that grows a
//! reply, and [`Frame::to_datagram`](crate::skey::event::Frame::to_datagram) is already
//! there to build one.
//!
//! # Cancellation and teardown
//!
//! Both ways of stopping are supported, in either order and repeatedly, as
//! [`ScannerBackend`](scanbus_core::ScannerBackend) requires: dropping the
//! [`Subscription`] and calling [`Listener::unsubscribe`] each remove the subscriber, and
//! whichever happens second finds nothing and does nothing.
//!
//! The difference between them is who waits for the file descriptor. `unsubscribe` is
//! `async` and **awaits the receive task**, so the socket is gone by the time it returns —
//! which is what makes a `Connect()`/`Disconnect()` cycle repeatable without ever
//! misreporting our own lingering socket as `brscan-skey`. `Drop` cannot await, so it
//! aborts the task and parks the handle in [`State::closing`] for the next
//! [`Listener::subscribe`] to await before it binds. Either way no rebind is attempted
//! while the previous socket may still be open.
//!
//! [`MobileBackend`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/scanbus-backend-mobile/src/lib.rs
//! [`registrar`]: crate::registrar

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::SystemTime;

use futures_core::Stream;
use scanbus_core::{ScanTrigger, ScannerId, TriggerId};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::skey::event::{Event, KeyPress};
use crate::skey::function::Function;

/// How many presses one subscriber may fall behind by.
///
/// The daemon's listener task awaits a whole scan per press ([`2.4`]), so a queue here is
/// a user pressing the key again while the previous job is still running — theirs, and
/// worth keeping. What it must not do is stall the receive loop, which serves every other
/// device on the machine: a press that finds the queue full is dropped with a warning
/// rather than blocking three other printers behind one that is scanning.
///
/// [`2.4`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/scanbus-daemon/src/listeners.rs
const EVENT_QUEUE: usize = 8;

/// How many recent presses stay correlatable by trigger id ([`Listener::take_press`]).
///
/// Bounded because the device is what decides how many arrive: a panel key pressed
/// repeatedly with nothing fetching the pages must not grow this without limit.
const RECORDED_PRESSES: usize = 16;

/// Longest datagram read in one go. `snmp_recv` hands `recvfrom` 2 KiB and the payloads
/// here are a few hundred bytes; anything longer is truncated by the kernel and then fails
/// the length check in [`Event::parse`], which is a dropped datagram and a debug line.
const RECV_BUFFER: usize = 2048;

/// Why the socket could not be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindError {
    /// Another process holds the port. Almost always `brscan-skey`; see the module
    /// documentation for why coexisting is not attempted.
    Taken { port: u16 },
    /// Anything else the bind refused.
    Io { port: u16, error: String },
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Taken { port } => write!(f, "UDP/{port} is already bound by another process"),
            Self::Io { port, error } => write!(f, "could not bind UDP/{port}: {error}"),
        }
    }
}

impl std::error::Error for BindError {}

impl BindError {
    fn from_io(port: u16, error: &std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::AddrInUse => Self::Taken { port },
            _ => Self::Io {
                port,
                error: error.to_string(),
            },
        }
    }
}

/// One panel press, kept so that the scan it asks for can be correlated back to it.
///
/// [`fetch_pages`](scanbus_core::ScannerBackend::fetch_pages) is handed a trigger id and
/// nothing else, and acquisition (5.10) needs to know which device and which function that
/// id was minted for. [`Listener::take_press`] is that seam; the trigger id is opaque to
/// everything above it, as the daemon's own job ids are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Press {
    /// The scanner the source address resolved to.
    pub scanner: ScannerId,
    /// The address the datagram came from.
    pub device: Ipv4Addr,
    /// The panel entry that was chosen.
    pub function: Function,
    /// `USER=` as the device echoed it — the name this host registered under.
    pub user: String,
    /// When the datagram was received.
    pub at: SystemTime,
}

/// The process-wide UDP/54925 listener.
///
/// Cheap to construct and opens nothing: the socket appears on the first
/// [`subscribe`](Self::subscribe) and goes away with the last subscriber.
#[derive(Debug)]
pub struct Listener {
    /// The port to ask for. [`LISTENER_PORT`](crate::skey::register::LISTENER_PORT) outside
    /// tests; `0` in them, where the kernel picks and [`Listener::port`] reports it back.
    requested_port: u16,
    /// Held across the bind, which is the one thing that cannot happen under the state
    /// lock: two subscribers arriving at once must not both try to open the socket, and
    /// the second would fail with `EADDRINUSE` against the first — the one failure this
    /// module is supposed to blame on `brscan-skey`.
    gate: tokio::sync::Mutex<()>,
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
struct State {
    /// The socket and the task reading it, while there is at least one subscriber.
    bound: Option<Bound>,
    /// The demultiplex table and the streams, one entry per subscriber. Its size is the
    /// refcount: the socket exists exactly while this is non-empty.
    subscribers: BTreeMap<ScannerId, Subscriber>,
    /// A receive task aborted by a [`Subscription`] drop, whose socket may not be closed
    /// yet. Awaited by the next bind — see the module documentation.
    closing: Option<JoinHandle<()>>,
    presses: VecDeque<(TriggerId, Press)>,
    next_trigger: u64,
    /// Stamped on each [`Subscriber`], so that a dropped [`Subscription`] can tell its own
    /// entry from the one that replaced it under the same [`ScannerId`].
    next_epoch: u64,
}

#[derive(Debug)]
struct Bound {
    port: u16,
    task: JoinHandle<()>,
}

#[derive(Debug)]
struct Subscriber {
    /// The address whose datagrams belong to this scanner.
    device: Ipv4Addr,
    /// Which [`Subscription`] this entry belongs to.
    epoch: u64,
    events: mpsc::Sender<ScanTrigger>,
}

impl State {
    /// The scanner a datagram from `from` belongs to.
    ///
    /// A linear scan: this is at most the number of Brother devices on one desk, and a
    /// second index by address would be one more thing to keep in step with the first.
    fn scanner_at(&self, from: Ipv4Addr) -> Option<(&ScannerId, &Subscriber)> {
        self.subscribers
            .iter()
            .find(|(_, subscriber)| subscriber.device == from)
    }

    /// Give up the socket, without waiting for the task to notice.
    ///
    /// Returns the aborted handle so the caller can decide whether to await it.
    fn close(&mut self) -> Option<JoinHandle<()>> {
        let bound = self.bound.take()?;
        bound.task.abort();
        debug!(port = bound.port, "released the Brother event socket");
        Some(bound.task)
    }

    fn record(&mut self, press: Press) -> TriggerId {
        self.next_trigger += 1;
        let id: TriggerId = format!("press-{}", self.next_trigger);
        self.presses.push_back((id.clone(), press));
        while self.presses.len() > RECORDED_PRESSES {
            if let Some((dropped, press)) = self.presses.pop_front() {
                debug!(
                    trigger = %dropped,
                    scanner = %press.scanner,
                    "forgetting a press nothing ever fetched the pages of",
                );
            }
        }
        id
    }
}

impl Listener {
    /// A listener for `port`. Binds nothing until something subscribes.
    pub fn new(port: u16) -> Self {
        Self {
            requested_port: port,
            gate: tokio::sync::Mutex::new(()),
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// The port the socket is actually bound to, or `None` while it is closed.
    pub fn port(&self) -> Option<u16> {
        self.lock().bound.as_ref().map(|bound| bound.port)
    }

    /// How many scanners are subscribed. Also the socket's refcount.
    pub fn subscribers(&self) -> usize {
        self.lock().subscribers.len()
    }

    /// Hand this scanner its own stream of presses, binding the socket if it is the first.
    ///
    /// Subscribing twice for one scanner replaces the previous stream rather than
    /// answering [`BackendError::Busy`](scanbus_core::BackendError::Busy): the daemon
    /// restarts a listener whose stream ended ([`crate::registrar`]'s caller), and a
    /// backend that refused the restart would leave the scanner connected to a stream
    /// nothing feeds. The replaced stream ends, which is what its owner is watching for.
    pub async fn subscribe(
        &self,
        scanner: ScannerId,
        device: Ipv4Addr,
    ) -> Result<Subscription, BindError> {
        let _gate = self.gate.lock().await;

        // Whatever a dropped subscription left behind, waited out here rather than
        // rebinding on top of a file descriptor that is still open. The guard is released
        // before the await on purpose: it is a `std::sync::Mutex`, so holding it across one
        // would make every future in this module `!Send`.
        let closing = self.lock().closing.take();
        if let Some(closing) = closing {
            let _ = closing.await;
        }

        if self.lock().bound.is_none() {
            let requested = self.requested_port;
            let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, requested))
                .await
                .map_err(|error| BindError::from_io(requested, &error))?;
            let port = socket
                .local_addr()
                .map_err(|error| BindError::from_io(requested, &error))?
                .port();

            let task = tokio::spawn(receive(socket, Arc::clone(&self.state)));
            info!(port, "listening for Brother panel presses");
            self.lock().bound = Some(Bound { port, task });
        }

        let (events, receiver) = mpsc::channel(EVENT_QUEUE);
        let epoch = {
            let mut state = self.lock();
            state.next_epoch += 1;
            let epoch = state.next_epoch;
            let previous = state.subscribers.insert(
                scanner.clone(),
                Subscriber {
                    device,
                    epoch,
                    events,
                },
            );
            if previous.is_some() {
                debug!(%scanner, %device, "replaced this scanner's press stream");
            }
            debug!(
                %scanner,
                %device,
                subscribers = state.subscribers.len(),
                "subscribed to panel presses",
            );
            epoch
        };

        Ok(Subscription {
            scanner,
            epoch,
            state: Arc::clone(&self.state),
            receiver,
        })
    }

    /// Stop this scanner's subscription, and close the socket if it was the last.
    ///
    /// A no-op for a scanner that is not subscribed, including one whose stream was
    /// already dropped — which is exactly what a `Disconnect()` after a crashed consumer
    /// looks like, and what the trait requires to be `Ok(())` rather than an error.
    ///
    /// Unlike the [`Drop`] path this **awaits** the receive task, so the file descriptor is
    /// closed by the time it returns.
    pub async fn unsubscribe(&self, scanner: &ScannerId) {
        let _gate = self.gate.lock().await;

        let closing = {
            let mut state = self.lock();
            let removed = state.subscribers.remove(scanner).is_some();
            if removed {
                debug!(%scanner, subscribers = state.subscribers.len(), "unsubscribed");
            }
            let closing = state.closing.take();
            if state.subscribers.is_empty() {
                state.close().or(closing)
            } else {
                closing
            }
        };
        if let Some(closing) = closing {
            let _ = closing.await;
        }
    }

    /// The press a trigger id was minted for, removed from the record.
    ///
    /// Removed rather than read because
    /// [`fetch_pages`](scanbus_core::ScannerBackend::fetch_pages) may be called only once
    /// per trigger, so a second call has to come back `UnknownJob` rather than start a
    /// second scan. Acquisition (5.10) is what calls this.
    pub fn take_press(&self, scanner: &ScannerId, trigger: &str) -> Option<Press> {
        let mut state = self.lock();
        let at = state
            .presses
            .iter()
            .position(|(id, press)| id == trigger && &press.scanner == scanner)?;
        state.presses.remove(at).map(|(_, press)| press)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("brother listener lock poisoned")
    }
}

/// One scanner's stream of presses.
///
/// Dropping it unsubscribes, so a consumer that goes away stops the demultiplex entry
/// without having to say so — and takes the socket with it when it was the last one.
#[derive(Debug)]
pub struct Subscription {
    scanner: ScannerId,
    /// Which entry in [`State::subscribers`] is this one's, so that dropping a stream that
    /// has already been replaced does not unsubscribe its replacement.
    epoch: u64,
    state: Arc<Mutex<State>>,
    receiver: mpsc::Receiver<ScanTrigger>,
}

impl Stream for Subscription {
    type Item = ScanTrigger;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().receiver.poll_recv(cx)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut state = self.state.lock().expect("brother listener lock poisoned");
        // Only if the entry is still ours: `unsubscribe` may have removed it already, and
        // `subscribe` for the same scanner may have replaced it — dropping a superseded
        // stream must not unsubscribe its successor.
        if state.subscribers.get(&self.scanner).map(|s| s.epoch) != Some(self.epoch) {
            return;
        }
        state.subscribers.remove(&self.scanner);
        if state.subscribers.is_empty() {
            debug_assert!(
                state.closing.is_none(),
                "a socket cannot be closing while another one is bound",
            );
            let closing = state.close();
            state.closing = closing;
        }
    }
}

/// A stream that stays open and never yields.
///
/// What a scanner with no panel presses to deliver listens on: a device with no IPv4
/// address to receive from, or a model that told us it does not know the registration OID.
/// Ending the stream immediately would be worse than useless — the daemon treats that as a
/// listener that died and restarts it until its budget runs out, leaving `Status="error"`
/// on a scanner that is working exactly as designed.
#[derive(Debug)]
pub struct Inert {
    /// Held only so the receiver never sees the channel close.
    _sender: mpsc::Sender<ScanTrigger>,
    receiver: mpsc::Receiver<ScanTrigger>,
}

impl Default for Inert {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel(1);
        Self {
            _sender: sender,
            receiver,
        }
    }
}

impl Stream for Inert {
    type Item = ScanTrigger;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().receiver.poll_recv(cx)
    }
}

/// The receive loop: one datagram at a time, for every device at once.
///
/// It owns the socket, so the file descriptor lives exactly as long as this future — which
/// is why both teardown paths are expressed in terms of this task rather than of a socket
/// somebody else holds a handle to.
async fn receive(socket: UdpSocket, state: Arc<Mutex<State>>) {
    let mut buffer = vec![0u8; RECV_BUFFER];
    loop {
        let (len, from) = match socket.recv_from(&mut buffer).await {
            Ok(received) => received,
            // Nothing recoverable is left once an unconnected UDP socket starts refusing
            // to be read: tokio has already retried what is retryable. Ending the loop
            // drops every subscriber's sender, so each stream ends and the daemon's own
            // backoff decides whether to ask for a new socket.
            Err(error) => {
                warn!(%error, "the Brother event socket failed; every press stream ends here");
                break;
            }
        };

        let SocketAddr::V4(from) = from else {
            // Nothing registers an IPv6 host: `HOST=` cannot carry one.
            debug!(%from, "ignoring a datagram from an IPv6 peer");
            continue;
        };

        dispatch(&state, *from.ip(), &buffer[..len]);
    }

    let mut state = state.lock().expect("brother listener lock poisoned");
    state.subscribers.clear();
    // The task being torn down is this one, so there is nothing left to abort or await;
    // dropping the handle detaches it. Clearing the slot is what lets the next
    // `subscribe` bind a fresh socket instead of finding a dead one.
    state.bound = None;
}

/// One datagram: what it is, whose it is, and what it becomes.
fn dispatch(state: &Arc<Mutex<State>>, from: Ipv4Addr, datagram: &[u8]) {
    let press = match Event::parse(datagram) {
        Ok(Event::KeyPress(press)) => press,
        // Somebody ran `brscan-skey --refresh` against the port we hold. Recognised and
        // declined, which is the whole reason `InternalCommand` exists.
        Ok(Event::Internal(command)) => {
            debug!(%from, ?command, "declining a brscan-skey CLI command on our socket");
            return;
        }
        Err(error) => {
            debug!(
                %from,
                %error,
                bytes = datagram.len(),
                "dropping a datagram that is not a panel press",
            );
            return;
        }
    };

    let mut state = state.lock().expect("brother listener lock poisoned");
    let Some((scanner, subscriber)) = state.scanner_at(from) else {
        // On a shared network this is another host's printer, or a registration from a
        // previous boot that has not lapsed yet. Neither is ours to complain about.
        debug!(
            %from,
            function = %press.function,
            "a panel press from an address no subscribed scanner matches; dropped",
        );
        return;
    };
    let scanner = scanner.clone();
    let events = subscriber.events.clone();

    log_disagreements(&scanner, from, &press);

    let function = press.function;
    let trigger = state.record(Press {
        scanner: scanner.clone(),
        device: from,
        function,
        user: press.user,
        at: SystemTime::now(),
    });
    let button = function.button_index();
    drop(state);

    match events.try_send(ScanTrigger::button(
        scanner.clone(),
        trigger.clone(),
        button,
    )) {
        Ok(()) => info!(%scanner, %function, button, %trigger, "panel press"),
        Err(mpsc::error::TrySendError::Full(_)) => warn!(
            %scanner,
            %function,
            queued = EVENT_QUEUE,
            "this scanner is not keeping up with its panel presses; dropping one",
        ),
        // The stream was dropped without the Drop handler running, which it cannot be —
        // but a lost press is not worth an `expect` on a socket anyone can send to.
        Err(mpsc::error::TrySendError::Closed(_)) => {
            debug!(%scanner, %function, "a press for a stream that is gone");
        }
    }
}

/// The two things a press can say that contradict what we know, logged and not acted on.
///
/// Neither changes which stream the press lands on: the source address is the identity
/// (see the module documentation), and `FUNC=` is what the vendor's own decoder trusts.
/// Both are here because they are diagnosable from nowhere else.
fn log_disagreements(scanner: &ScannerId, from: Ipv4Addr, press: &KeyPress) {
    if press.client != from {
        debug!(
            %scanner,
            %from,
            client = %press.client,
            "the device reports an address it is not sending from; something is translating",
        );
    }
    if press.appnum_agrees() == Some(false) {
        warn!(
            %scanner,
            function = %press.function,
            appnum = press.appnum,
            "this press names its function twice and disagrees; trusting FUNC",
        );
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use scanbus_core::TriggerKind;
    use tokio::time::timeout;

    use super::*;
    use crate::skey::event::Frame;

    /// How long a test waits for a datagram to cross the loopback before giving up.
    const DELIVERY: Duration = Duration::from_secs(5);

    fn scanner(host: u8) -> ScannerId {
        ScannerId::from_backend("brother", &format!("mfc-{host}")).unwrap()
    }

    /// A loopback address per device: `127.0.0.x` is bindable and gives every fake printer
    /// a source address of its own, which is what demultiplexing is being tested on.
    fn device(host: u8) -> Ipv4Addr {
        Ipv4Addr::new(127, 0, 0, host)
    }

    fn key_press(from: Ipv4Addr, function: Function) -> Vec<u8> {
        let payload = format!(
            "TYPE=BR;BUTTON=SCAN;USER=\"desktop\";FUNC={};HOST=127.0.0.1:54925;APPNUM={};\
             REGID=1;SEQ=1;CLIENT={from}",
            function.as_str(),
            function.appnum(),
        );
        Frame {
            id: 0x01,
            code: 0x01,
            payload: &payload,
        }
        .to_datagram()
    }

    /// A printer at `from`, sending to the listener.
    async fn send(from: Ipv4Addr, port: u16, datagram: &[u8]) {
        let socket = UdpSocket::bind(SocketAddrV4::new(from, 0)).await.unwrap();
        socket
            .send_to(datagram, SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
    }

    async fn next(subscription: &mut Subscription) -> ScanTrigger {
        timeout(DELIVERY, futures_next(subscription))
            .await
            .expect("a press should arrive")
    }

    /// `StreamExt::next` without pulling in `futures-util` for one call.
    async fn futures_next(subscription: &mut Subscription) -> ScanTrigger {
        std::future::poll_fn(|cx| Pin::new(&mut *subscription).poll_next(cx))
            .await
            .expect("the stream must not end")
    }

    /// How many UDP sockets this process holds on `port`, from `/proc`.
    ///
    /// 5.9's acceptance criterion is written in terms of `/proc/self/fd`, so it is asserted
    /// from there rather than from this module's own bookkeeping, which is the thing under
    /// test. `/proc/self/fd` gives the socket *inodes* the process owns and nothing about
    /// what they are bound to, so `/proc/net/udp` supplies the port — and counting by port
    /// rather than counting sockets is what makes the number immune to the other tests in
    /// this binary, which run in parallel threads of the same process and open sockets of
    /// their own.
    fn sockets_on(port: u16) -> usize {
        let inodes: std::collections::BTreeSet<String> = std::fs::read_dir("/proc/self/fd")
            .expect("/proc/self/fd is readable on Linux")
            .filter_map(Result::ok)
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .filter_map(|target| {
                target
                    .to_string_lossy()
                    .strip_prefix("socket:[")?
                    .strip_suffix(']')
                    .map(str::to_owned)
            })
            .collect();

        // `sl local_address rem_address st tx:rx tr:when retrnsmt uid timeout inode …`,
        // with `local_address` as hex `address:port` in field 1 and the inode in field 9.
        // Matching the whole of `0.0.0.0:port` and not just the port is what keeps the
        // sockets the other tests bind to `127.0.0.x:0` out of the count.
        let wanted = format!("00000000:{port:04X}");
        std::fs::read_to_string("/proc/net/udp")
            .expect("/proc/net/udp is readable on Linux")
            .lines()
            .skip(1)
            .filter(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                let (Some(local), Some(inode)) = (fields.get(1), fields.get(9)) else {
                    return false;
                };
                local.eq_ignore_ascii_case(&wanted) && inodes.contains(*inode)
            })
            .count()
    }

    /// A port for a test that needs a fixed one, free at the moment it is handed out.
    ///
    /// Taken from **below** the ephemeral range and from a counter no two tests read the
    /// same value of, because the obvious implementation — bind port 0, read the port back,
    /// close the socket — hands out a port the kernel is about to hand to the next binder
    /// too. That collides with the other tests in this binary, which run in parallel threads
    /// and bind ephemeral ports of their own, and it made `sockets_on` count somebody else's
    /// socket.
    async fn free_port() -> u16 {
        static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(24_925);

        for _ in 0..64 {
            let port = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let probe = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)).await;
            if probe.is_ok() {
                drop(probe);
                return port;
            }
        }
        panic!("no free port for this test in 64 tries");
    }

    /// Every panel entry, from one device, on the one socket.
    #[tokio::test]
    async fn a_press_becomes_a_trigger_carrying_its_button_index() {
        let listener = Listener::new(0);
        let mut subscription = listener.subscribe(scanner(1), device(1)).await.unwrap();
        let port = listener.port().unwrap();

        for function in Function::ALL {
            send(device(1), port, &key_press(device(1), function)).await;
            let trigger = next(&mut subscription).await;

            assert_eq!(trigger.scanner_id, scanner(1));
            assert_eq!(
                trigger.kind,
                TriggerKind::Button {
                    index: function.button_index()
                },
                "{function} must arrive as its own button",
            );

            // The id `fetch_pages` will be handed, and what it resolves back to.
            let press = listener
                .take_press(&scanner(1), &trigger.id)
                .expect("the trigger id must correlate to the press it was minted for");
            assert_eq!(press.function, function);
            assert_eq!(press.device, device(1));
            assert_eq!(press.user, "desktop");
            // Once only: a second `fetch_pages` for one job is not a second scan.
            assert_eq!(listener.take_press(&scanner(1), &trigger.id), None);
            // …and not another scanner's, however well the id is guessed.
            assert_eq!(listener.take_press(&scanner(2), &trigger.id), None);
        }
    }

    /// Two devices, one socket, and a press that reaches only its own stream.
    #[tokio::test]
    async fn two_devices_share_one_socket_and_do_not_cross_streams() {
        let listener = Listener::new(0);
        let mut first = listener.subscribe(scanner(1), device(1)).await.unwrap();
        let port = listener.port().unwrap();
        let mut second = listener.subscribe(scanner(2), device(2)).await.unwrap();

        assert_eq!(
            listener.port(),
            Some(port),
            "the second subscriber must not open a second socket",
        );
        assert_eq!(listener.subscribers(), 2);

        send(device(2), port, &key_press(device(2), Function::Ocr)).await;
        let trigger = next(&mut second).await;
        assert_eq!(trigger.scanner_id, scanner(2));
        assert_eq!(trigger.kind, TriggerKind::Button { index: 2 });

        send(device(1), port, &key_press(device(1), Function::Email)).await;
        let trigger = next(&mut first).await;
        assert_eq!(trigger.scanner_id, scanner(1));
        assert_eq!(trigger.kind, TriggerKind::Button { index: 3 });

        // Neither stream received the other's press: each has exactly one, now taken.
        assert!(
            timeout(Duration::from_millis(50), futures_next(&mut first))
                .await
                .is_err(),
        );
        assert!(
            timeout(Duration::from_millis(50), futures_next(&mut second))
                .await
                .is_err(),
        );
    }

    /// A datagram from an address nothing matches, and one that is not a press at all.
    /// Neither may disturb a subscribed scanner's stream.
    #[tokio::test]
    async fn unknown_sources_and_garbage_are_dropped_without_disturbing_anybody() {
        let listener = Listener::new(0);
        let mut subscription = listener.subscribe(scanner(1), device(1)).await.unwrap();
        let port = listener.port().unwrap();

        // Somebody else's printer, saying exactly the right thing from the wrong address.
        send(device(9), port, &key_press(device(9), Function::Image)).await;
        // The vendor CLI talking to a daemon that is not there.
        send(
            device(1),
            port,
            &Frame {
                id: 0x80,
                code: 0x84,
                payload: "Refresh Device List",
            }
            .to_datagram(),
        )
        .await;
        // Bytes that are not a frame.
        send(device(1), port, b"\xff\xff\xff\xff\xff").await;
        send(device(1), port, &[]).await;

        // …and then a real press from the real device, which proves the loop survived all
        // of the above rather than that nothing arrived.
        send(device(1), port, &key_press(device(1), Function::File)).await;
        let trigger = next(&mut subscription).await;
        assert_eq!(trigger.scanner_id, scanner(1));
        assert_eq!(trigger.kind, TriggerKind::Button { index: 0 });
    }

    /// The socket exists exactly while somebody is subscribed, over and over.
    ///
    /// This is the acceptance criterion that has to be run rather than reasoned about: a
    /// rebind that raced our own closing socket would come back `EADDRINUSE` and be
    /// reported as `brscan-skey`, on a machine that has never had it installed.
    #[tokio::test]
    async fn ten_connect_disconnect_cycles_leak_no_socket() {
        // A fixed port for the whole run, so that "one socket, then none" is asserted about
        // the same port every cycle.
        let port = free_port().await;
        let listener = Listener::new(port);
        assert_eq!(sockets_on(port), 0, "the port must start out unused");

        for cycle in 0..10 {
            let mut subscription = listener.subscribe(scanner(1), device(1)).await.unwrap();
            assert_eq!(listener.port(), Some(port), "cycle {cycle} must bind");
            assert_eq!(
                sockets_on(port),
                1,
                "cycle {cycle} must hold exactly one socket",
            );

            send(device(1), port, &key_press(device(1), Function::Image)).await;
            let trigger = next(&mut subscription).await;
            assert_eq!(trigger.kind, TriggerKind::Button { index: 1 });

            // Both teardown paths, alternating, and both must leave the same state.
            if cycle % 2 == 0 {
                listener.unsubscribe(&scanner(1)).await;
                drop(subscription);
            } else {
                drop(subscription);
                listener.unsubscribe(&scanner(1)).await;
            }

            assert_eq!(listener.subscribers(), 0, "cycle {cycle}");
            assert_eq!(listener.port(), None, "cycle {cycle} must release the port");
            assert_eq!(sockets_on(port), 0, "cycle {cycle} left a socket behind");
        }
    }

    /// A dropped stream releases the socket without an `unsubscribe`, and the next
    /// subscriber may bind the same port immediately.
    #[tokio::test]
    async fn a_dropped_stream_releases_the_port_for_the_next_subscriber() {
        // A fixed port, so that "the next bind gets the same port" is a real claim: with
        // port 0 the kernel would just hand out another one.
        let port = free_port().await;
        let listener = Listener::new(port);
        let subscription = listener.subscribe(scanner(1), device(1)).await.unwrap();
        assert_eq!(listener.port(), Some(port));
        drop(subscription);

        // No unsubscribe, no yield, no sleep: the next bind is what waits for the socket.
        let mut subscription = listener.subscribe(scanner(1), device(1)).await.unwrap();
        assert_eq!(listener.port(), Some(port));

        send(device(1), port, &key_press(device(1), Function::Image)).await;
        assert_eq!(
            next(&mut subscription).await.kind,
            TriggerKind::Button { index: 1 },
        );
    }

    /// Unsubscribing something that is not subscribed, twice, and in both orders.
    #[tokio::test]
    async fn stopping_twice_and_in_either_order_is_not_an_error() {
        let listener = Listener::new(0);

        // Before anything was ever subscribed.
        listener.unsubscribe(&scanner(1)).await;

        let subscription = listener.subscribe(scanner(1), device(1)).await.unwrap();
        listener.unsubscribe(&scanner(1)).await;
        listener.unsubscribe(&scanner(1)).await;
        drop(subscription);
        listener.unsubscribe(&scanner(1)).await;

        assert_eq!(listener.subscribers(), 0);
        assert_eq!(listener.port(), None);
    }

    /// One scanner's second subscription replaces the first, and the socket stays put.
    #[tokio::test]
    async fn resubscribing_replaces_the_stream_without_rebinding() {
        let listener = Listener::new(0);
        let first = listener.subscribe(scanner(1), device(1)).await.unwrap();
        let port = listener.port().unwrap();

        let mut second = listener.subscribe(scanner(1), device(1)).await.unwrap();
        assert_eq!(listener.port(), Some(port), "no rebind");
        assert_eq!(listener.subscribers(), 1);

        send(device(1), port, &key_press(device(1), Function::Image)).await;
        assert_eq!(
            next(&mut second).await.kind,
            TriggerKind::Button { index: 1 },
        );

        // Dropping the superseded stream must not unsubscribe its successor.
        drop(first);
        assert_eq!(listener.subscribers(), 1);
        assert_eq!(listener.port(), Some(port));

        send(device(1), port, &key_press(device(1), Function::Ocr)).await;
        assert_eq!(
            next(&mut second).await.kind,
            TriggerKind::Button { index: 2 }
        );
    }

    /// A port somebody else holds is `Taken`, and says nothing about our own state.
    #[tokio::test]
    async fn a_port_another_process_holds_is_reported_as_taken() {
        let squatter = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
            .await
            .unwrap();
        let port = squatter.local_addr().unwrap().port();

        let listener = Listener::new(port);
        let error = listener
            .subscribe(scanner(1), device(1))
            .await
            .expect_err("a port somebody else holds cannot be bound");
        assert_eq!(error, BindError::Taken { port });
        assert_eq!(listener.subscribers(), 0, "a failed bind subscribes nobody");
        assert_eq!(listener.port(), None);

        // And it recovers the moment the port is free, with no state to reset.
        drop(squatter);
        let subscription = listener.subscribe(scanner(1), device(1)).await;
        assert!(subscription.is_ok(), "{subscription:?}");
    }

    /// The record of recent presses is bounded, and drops the oldest first.
    #[tokio::test]
    async fn the_press_record_forgets_the_oldest_rather_than_growing() {
        let listener = Listener::new(0);
        let mut subscription = listener.subscribe(scanner(1), device(1)).await.unwrap();
        let port = listener.port().unwrap();

        let mut triggers = Vec::new();
        for _ in 0..(RECORDED_PRESSES + 4) {
            send(device(1), port, &key_press(device(1), Function::Image)).await;
            triggers.push(next(&mut subscription).await.id);
        }

        let (forgotten, kept) = triggers.split_at(4);
        for trigger in forgotten {
            assert_eq!(
                listener.take_press(&scanner(1), trigger),
                None,
                "{trigger} is older than the record keeps",
            );
        }
        for trigger in kept {
            assert!(
                listener.take_press(&scanner(1), trigger).is_some(),
                "{trigger} should still be correlatable",
            );
        }
    }

    /// The stream a subscriber is not draining fills up, and the loop keeps serving the
    /// other device rather than stalling behind it.
    #[tokio::test]
    async fn a_subscriber_that_stopped_reading_does_not_stall_the_other_devices() {
        let listener = Listener::new(0);
        let _stalled = listener.subscribe(scanner(1), device(1)).await.unwrap();
        let port = listener.port().unwrap();
        let mut healthy = listener.subscribe(scanner(2), device(2)).await.unwrap();

        for _ in 0..(EVENT_QUEUE * 2) {
            send(device(1), port, &key_press(device(1), Function::Image)).await;
        }
        send(device(2), port, &key_press(device(2), Function::File)).await;

        assert_eq!(
            next(&mut healthy).await.kind,
            TriggerKind::Button { index: 0 },
            "the second device's press must arrive whatever the first one is doing",
        );
    }

    /// An inert stream stays open forever rather than ending, which the daemon would read
    /// as a listener that died.
    #[tokio::test]
    async fn an_inert_stream_never_ends() {
        let mut inert = Inert::default();
        let poll = timeout(Duration::from_millis(50), async {
            std::future::poll_fn(|cx| Pin::new(&mut inert).poll_next(cx)).await
        })
        .await;
        assert!(poll.is_err(), "an inert stream must stay pending");
    }
}
