//! Following a property without racing the call that changes it.
//!
//! [`scanbus-cli.md`] §7 states the problem: a `pair --wait` that calls `Pair()` and
//! *then* subscribes to `PropertiesChanged` loses the transition on a scanner whose
//! backend is already installed — `PairingState` goes `pairing → done` before the
//! subscription exists, and the client waits out its timeout on a scanner that paired
//! successfully. The sequence that closes it is four steps:
//!
//! 1. create the signal stream
//! 2. make the call
//! 3. read the current state once
//! 4. consume the stream, treating step 3's snapshot as the first event
//!
//! [`PropertyWatch`] is steps 1, 3 and 4, split so that step 2 has to happen between
//! them: [`PropertyWatch::subscribe`] returns a value whose only useful method consumes
//! it into the snapshot and the stream, so a caller cannot express the broken ordering
//! without noticing.
//!
//! # Step 4 is where the subtlety actually is
//!
//! Subscribing first means the stream may hold signals that predate the snapshot. Left
//! alone they are replayed *after* it — a client that already read `done` would then be
//! told `pairing`, and a `--wait` looking for a terminal state would stop at the wrong
//! one. So the snapshot drops them, and it can do so exactly because D-Bus delivers
//! messages on one connection in order: every signal the daemon emitted before it handled
//! the `GetAll` arrives before the `GetAll` reply, so anything already buffered when the
//! reply lands is by definition reflected in the snapshot. Anything that arrives later is
//! genuinely newer. `now_or_never` is what asks "already buffered?" without waiting.
//!
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

use std::collections::HashMap;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::future::FutureExt as _;
use futures_util::stream::{Stream, StreamExt as _};
use zbus::Connection;
use zbus::fdo::{PropertiesChanged, PropertiesChangedStream, PropertiesProxy};
use zbus::names::InterfaceName;
use zbus::zvariant::ObjectPath;

use crate::convert::Dict;
use crate::error::{Error, Result};
use crate::scanner::ScannerState;

/// The `org.scanbus.Scanner1` interface name, spelled once — beside the proxies.
pub use crate::proxy::SCANNER_INTERFACE;

/// A subscription to one object's `PropertiesChanged`, before anything has been read.
///
/// Step 1 of the sequence above. Make the call that changes something, then
/// [`PropertyWatch::snapshot`].
pub struct PropertyWatch {
    properties: PropertiesProxy<'static>,
    interface: InterfaceName<'static>,
    changes: PropertiesChangedStream,
}

impl PropertyWatch {
    /// Subscribes to `interface`'s changes on `path`. Reads nothing.
    ///
    /// # Errors
    ///
    /// [`Error::Bus`] if the path or the interface name is malformed, or the match rule
    /// cannot be installed.
    pub async fn subscribe<'p, P>(connection: &Connection, path: P, interface: &str) -> Result<Self>
    where
        P: TryInto<ObjectPath<'p>>,
        P::Error: Into<zbus::Error>,
    {
        let interface = InterfaceName::try_from(interface.to_owned())
            .map_err(|error| Error::Bus(zbus::Error::Names(error)))?;
        let properties = crate::proxy::properties(connection, path).await?;

        // Awaited here, not in `snapshot`: when this returns, the match rule is
        // installed on the bus, which is what makes "subscribe before you call" true
        // rather than merely intended.
        let changes = properties.receive_properties_changed().await?;

        Ok(Self {
            properties,
            interface,
            changes,
        })
    }

    /// Steps 3 and 4: the current property map, and the changes strictly after it.
    ///
    /// # Errors
    ///
    /// [`Error::Bus`] if `GetAll` fails — including
    /// `org.freedesktop.DBus.Error.UnknownObject` for an object that went away between
    /// the subscription and here, which for a discovered scanner or a job is ordinary.
    pub async fn snapshot(self) -> Result<(Dict, PropertyChanges)> {
        let Self {
            properties,
            interface,
            changes,
        } = self;

        let snapshot = properties.get_all(interface.as_ref()).await?;

        let mut changes = PropertyChanges {
            stream: Box::pin(changes),
            interface,
        };

        // Everything already queued predates the reply, and therefore the snapshot — see
        // the module documentation. Dropping it is what makes the snapshot the first
        // event rather than a value the stream immediately contradicts.
        while changes.next().now_or_never().flatten().is_some() {}

        Ok((snapshot, changes))
    }
}

/// The `PropertiesChanged` signals of one interface on one object.
///
/// Filtered by interface: a `Scanner1` path also carries `Properties`, `Introspectable`
/// and — after [2.5] — nothing else, but a watcher told to follow `Scanner1` must not be
/// woken by a sibling interface's signal and conclude a scanner changed.
///
/// [2.5]: https://github.com/jeanparpaillon/scanbus_service/issues/9
pub struct PropertyChanges {
    stream: Pin<Box<PropertiesChangedStream>>,
    interface: InterfaceName<'static>,
}

impl Stream for PropertyChanges {
    type Item = PropertiesChanged;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            let signal = match this.stream.as_mut().poll_next(cx) {
                Poll::Ready(Some(signal)) => signal,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            };

            // A signal whose body does not deserialise is not ours to interpret; the
            // stream is a view of one interface, and skipping is what "not ours" means.
            let ours = signal
                .args()
                .is_ok_and(|args| args.interface_name == this.interface);
            if ours {
                return Poll::Ready(Some(signal));
            }
        }
    }
}

/// A subscription to one `Scanner1` object, before anything has been read.
///
/// The typed half of [`PropertyWatch`]: same three steps, but the stream yields whole
/// [`ScannerState`]s rather than signal bodies, so a `pair --wait` matches on
/// [`scanbus_core::PairingState`] variants instead of on strings.
pub struct ScannerWatch {
    inner: PropertyWatch,
}

impl ScannerWatch {
    /// Subscribes to a scanner's changes. Reads nothing.
    ///
    /// # Errors
    ///
    /// As [`PropertyWatch::subscribe`].
    pub async fn subscribe<'p, P>(connection: &Connection, path: P) -> Result<Self>
    where
        P: TryInto<ObjectPath<'p>>,
        P::Error: Into<zbus::Error>,
    {
        Ok(Self {
            inner: PropertyWatch::subscribe(connection, path, SCANNER_INTERFACE).await?,
        })
    }

    /// The states of this scanner, starting with the one it is in now.
    ///
    /// The first item is always the snapshot — which is what makes waiting for a
    /// terminal `PairingState` terminate on a scanner that reached it before anyone
    /// looked. Afterwards, one item per `PropertiesChanged` that actually moves
    /// something: a signal re-announcing a value the caller already has is dropped, so a
    /// `--wait` loop sees transitions and not heartbeats.
    ///
    /// # Errors
    ///
    /// As [`PropertyWatch::snapshot`], plus [`Error::Decode`] if the snapshot is not a
    /// `Scanner1` this version understands.
    pub async fn states(self) -> Result<ScannerStates> {
        let (snapshot, changes) = self.inner.snapshot().await?;
        let state = ScannerState::from_properties(&snapshot)?;

        Ok(ScannerStates {
            changes,
            current: state.clone(),
            first: Some(state),
        })
    }
}

/// A stream of whole [`ScannerState`]s: the snapshot, then each change.
pub struct ScannerStates {
    changes: PropertyChanges,
    /// The last state yielded, which each delta is applied onto.
    current: ScannerState,
    /// The snapshot, until it has been handed out.
    first: Option<ScannerState>,
}

impl ScannerStates {
    /// The state as of the last item yielded, without waiting for another.
    pub fn current(&self) -> &ScannerState {
        &self.current
    }
}

impl Stream for ScannerStates {
    type Item = Result<ScannerState>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if let Some(first) = this.first.take() {
            return Poll::Ready(Some(Ok(first)));
        }

        loop {
            let signal = match Pin::new(&mut this.changes).poll_next(cx) {
                Poll::Ready(Some(signal)) => signal,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            };

            let args = match signal.args() {
                Ok(args) => args,
                Err(error) => return Poll::Ready(Some(Err(Error::Bus(error)))),
            };

            // The daemon emits every §3 property with its value, which is what lets a
            // client follow a scanner from the signals alone. An invalidation says that
            // stopped being true; reporting it beats carrying on with a value that is
            // now a guess. See `Error::Invalidated`.
            if let Some(property) = args.invalidated_properties.first() {
                return Poll::Ready(Some(Err(Error::Invalidated {
                    property: (*property).to_owned(),
                })));
            }

            let mut next = this.current.clone();
            if let Err(error) = next.apply(&args.changed_properties) {
                return Poll::Ready(Some(Err(error.into())));
            }

            // A `PropertiesChanged` that changes nothing this version tracks is noise —
            // and so is one re-announcing a value the caller was already given.
            if next == this.current {
                continue;
            }

            this.current = next.clone();
            return Poll::Ready(Some(Ok(next)));
        }
    }
}

/// `Scanner1` objects announced by `InterfacesAdded` on `/org/scanbus`, decoded.
///
/// `discover` ([`scanbus-cli.md`] §7) needs the same raceless shape as one object's
/// properties — subscribe before `StartDiscovery`, then read `GetManagedObjects` once it
/// returns — one level up, on the whole tree instead of one path. This is step 1 for
/// that case: every `InterfacesAdded` reaches a client, buttons and jobs included, and
/// this filters to the ones carrying a `Scanner1`, the same way [`PropertyChanges`]
/// filters one interface out of several sharing a path.
///
/// Kept out of the CLI on purpose: the alternative is `scanbus-cli` naming
/// `zbus::fdo::InterfacesAdded` and `zbus::zvariant::OwnedValue` itself, which is the
/// `zbus` dependency [`scanbus-cli.md`] §2 says stays out of that crate's manifest.
///
/// [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md
pub struct ScannerAdditions {
    stream: zbus::fdo::InterfacesAddedStream,
}

impl ScannerAdditions {
    /// Subscribes to every `InterfacesAdded` under `/org/scanbus`. Reads nothing.
    ///
    /// # Errors
    ///
    /// [`Error::Bus`] if the match rule cannot be installed.
    pub async fn subscribe(connection: &Connection) -> Result<Self> {
        let manager = crate::proxy::object_manager(connection).await?;
        let stream = manager.receive_interfaces_added().await?;
        Ok(Self { stream })
    }
}

impl Stream for ScannerAdditions {
    type Item = Result<ScannerState>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            let signal = match Pin::new(&mut this.stream).poll_next(cx) {
                Poll::Ready(Some(signal)) => signal,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            };

            let args = match signal.args() {
                Ok(args) => args,
                Err(error) => return Poll::Ready(Some(Err(Error::Bus(error)))),
            };

            let Some(properties) = args.interfaces_and_properties().get(SCANNER_INTERFACE) else {
                // A button or a job gaining its interface — not ours.
                continue;
            };

            let dict = to_dict(properties);
            return Poll::Ready(Some(ScannerState::from_properties(&dict).map_err(Error::from)));
        }
    }
}

/// Reads one `InterfacesAdded`'s properties into an owned `a{sv}`.
///
/// A value that fails to clone — only possible for a file descriptor, which nothing in
/// this API ever sends — is dropped rather than failing the whole event: a scanner
/// missing one such key is still a scanner worth reporting, and [`ScannerState`] treats
/// a missing key exactly like the daemon having omitted it.
fn to_dict(properties: &HashMap<&str, zbus::zvariant::Value<'_>>) -> Dict {
    properties
        .iter()
        .filter_map(|(key, value)| {
            let owned = zbus::zvariant::OwnedValue::try_from(value.try_clone().ok()?).ok()?;
            Some(((*key).to_owned(), owned))
        })
        .collect()
}
