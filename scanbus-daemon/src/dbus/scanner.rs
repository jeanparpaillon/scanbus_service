//! [`Scanner1`]: one object per scanner, paired or not, and pairing that never blocks.
//!
//! §1 of [`scanbus-dbus-api.md`] refuses two representations for one scanner: a device
//! found by discovery is the *same* kind of object as a paired one, with `Paired=false`,
//! so a client calls `Pair()` on the object it just saw appear instead of translating a
//! struct into a path. That is the decision this type implements, and the reason the
//! registry ([`crate::scanners`]) has to remember why each object exists.
//!
//! # `Pair()` starts a task; it does not do the work
//!
//! §9 is unambiguous: no D-Bus call blocks for the duration of a package install. A
//! handler that awaited `ensure_installed` would hold the reply for as long as a Brother
//! `.deb` takes to download, and the default client timeout of 25 s would report a
//! failure for a scanner that went on to pair successfully. So all three methods here
//! are thin: they poke [`PairingMachine`], which owns the sequence, and return.
//!
//! Everything a client learns about the outcome arrives as `PropertiesChanged`, emitted
//! by [`supervise`] — **one task per object, and the only writer of `PairingState`,
//! `PairingError` and `Paired`**. The alternative, emitting from wherever a state happens
//! to change, is what leaves a property updated in memory with no client told: there is
//! no `&mut self` in a spawned pairing task to hang a `*_changed` call off, so the
//! emission has to be driven from the transition stream or not at all.
//!
//! # `Connect()` starts nothing it owns
//!
//! The listener task belongs to [`ScannerRegistry`], not to this object: `Connect()` asks
//! it to *ensure* one exists and `Disconnect()` asks it to stop, so two `Connect()` calls
//! — or a `Connect()` after the pairing that already started one — converge on a single
//! task instead of racing to create a second. [`crate::listeners`] is where that decision
//! is argued.
//!
//! What lives here is the state a client can see or set: `Connected`, which is exactly
//! "the registry has a listener task for this scanner", and the session profile of §3,
//! which is not a property at all — it is per connection, it dies with `Disconnect()`,
//! and publishing it would invite a client to set it out of band.
//!
//! # Every method takes `&self`, and that is load-bearing
//!
//! zbus holds a read lock on the interface for the whole of a `&self` method and a
//! **write** lock for a `&mut self` one (see [`zbus::object_server::InterfaceRef::get`]).
//! A write lock anywhere here would deadlock the daemon, because two of the locks in play
//! are taken in opposite orders:
//!
//! - [`ScannerRegistry`] holds its state lock while it emits `PropertiesChanged` on an
//!   object — it takes the registry lock, then the interface lock;
//! - `Unpair()` runs inside the interface lock and then asks the registry to retire the
//!   object — the interface lock, then the registry lock.
//!
//! Read locks are shared, so as things stand the second waits for nothing and the cycle
//! never closes. Add one `&mut self` method — or one
//! [`get_mut`](zbus::object_server::InterfaceRef::get_mut) — and a rediscovery landing
//! during an `Unpair()` hangs both tasks for good. The state that has to change therefore
//! lives behind [`Scanner1`]'s own locks, not behind zbus's: [`PairingMachine`] for
//! everything about pairing, a [`Mutex`] for `DefaultProfile`.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use scanbus_core::{
    PairOutcome, PairingMachine, PairingState, ProfileKind, ScannerId, ScannerInfo, Status,
    UnpairError,
};
use tokio::sync::broadcast;
use tracing::{debug, info, instrument, warn};
use zbus::names::InterfaceName;
use zbus::object_server::InterfaceRef;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::dbus::convert::{self, Dict};
use crate::dbus::error::ScanbusError;
use crate::dbus::objects::ObjectRegistry;
use crate::listeners::ListenError;
use crate::scanners::{Retired, ScannerRegistry};

/// The interface name, spelled once: [`emit`] builds its own `PropertiesChanged`.
pub const INTERFACE: &str = "org.scanbus.Scanner1";

/// The one `Connect()` option §3 documents: `{"profile": "document"}`.
const PROFILE_OPTION: &str = "profile";

/// The `org.scanbus.Scanner1` object of §3.
///
/// Holds no scanner state of its own beyond `DefaultProfile`: [`ScannerInfo`], `Paired`
/// and the pairing state all come out of [`PairingMachine`], so there is nothing here
/// that can disagree with what the machine is doing.
pub struct Scanner1 {
    machine: Arc<PairingMachine>,
    /// The registry that published this object, for `Unpair()` to retire it through.
    ///
    /// Weak on purpose: the registry owns the [`ObjectRegistry`], which owns the
    /// connection, whose object server owns this interface. A strong handle would close
    /// that loop and keep the whole daemon alive after a shutdown that dropped it.
    scanners: Weak<ScannerRegistry>,
    /// `None` is the `""` of §3: data received with no profile attached is left raw.
    default_profile: Mutex<Option<ProfileKind>>,
    /// The `{"profile": …}` of the `Connect()` that opened the current session.
    ///
    /// Not a D-Bus property: §3 gives it to `Connect()` as an option, it ranks *between*
    /// `Button1.Profile` and `DefaultProfile` (see
    /// [`ButtonEvent::profile`](crate::listeners::ButtonEvent::profile)), and it is
    /// cleared by `Disconnect()`. A property would outlive the connection it belongs to.
    session_profile: Mutex<Option<ProfileKind>>,
    /// Whether the registry is holding a listener task for this scanner.
    ///
    /// The registry is the only writer, through [`set_connected`] — the same
    /// one-writer-per-property rule that makes [`supervise`] the only source of `Paired`.
    connected: Mutex<bool>,
}

impl Scanner1 {
    /// A scanner object driven by `machine`, retiring itself through `scanners`.
    pub fn new(machine: Arc<PairingMachine>, scanners: Weak<ScannerRegistry>) -> Self {
        Self {
            machine,
            scanners,
            default_profile: Mutex::new(None),
            session_profile: Mutex::new(None),
            connected: Mutex::new(false),
        }
    }

    /// What the backend last reported.
    pub fn info(&self) -> ScannerInfo {
        self.machine.scanner()
    }

    /// Whether this scanner is paired with this host.
    ///
    /// Named apart from the `Paired` property getter below, which the interface macro
    /// owns; both read the same field of the machine.
    pub fn is_paired(&self) -> bool {
        self.machine.is_paired()
    }

    /// Where the pairing process is right now.
    ///
    /// Not named `pairing_state`: that one belongs to the interface macro below, and two
    /// inherent methods of one type cannot share a name.
    pub fn state(&self) -> PairingState {
        self.machine.state()
    }

    /// The machine this object exports.
    pub fn machine(&self) -> &Arc<PairingMachine> {
        &self.machine
    }

    /// `DefaultProfile` as a [`ProfileKind`] rather than as the `""`-for-none string the
    /// property carries.
    ///
    /// Named apart from the `DefaultProfile` getter below, which the interface macro
    /// owns; both read the same field.
    pub fn default_profile_value(&self) -> Option<ProfileKind> {
        *self
            .default_profile
            .lock()
            .expect("default profile lock poisoned")
    }

    /// The profile the current `Connect()` asked for, if it asked for one.
    pub fn session_profile(&self) -> Option<ProfileKind> {
        *self
            .session_profile
            .lock()
            .expect("session profile lock poisoned")
    }

    /// Whether a listener task is running for this scanner.
    ///
    /// Named apart from the `Connected` property getter below, which the interface macro
    /// owns; both read the same field.
    pub fn is_connected(&self) -> bool {
        *self.connected.lock().expect("connected lock poisoned")
    }
}

#[zbus::interface(name = "org.scanbus.Scanner1")]
impl Scanner1 {
    /// Stable identifier — also the `{id}` element of this object's path.
    #[zbus(property)]
    fn id(&self) -> String {
        self.info().id.to_string()
    }

    /// Human-readable name, as the device reports it.
    #[zbus(property)]
    fn name(&self) -> String {
        self.info().name
    }

    /// Which subsystem found it: `"sane"`, `"escl"`, `"proprietary:brother"`, …
    ///
    /// Kept on the object precisely so a client can tell which backend won a
    /// deduplication (§9); see [`crate::scanners`] for the rule that decides it.
    #[zbus(property)]
    fn backend(&self) -> String {
        self.info().backend
    }

    /// Connection URI or device path.
    #[zbus(property)]
    fn address(&self) -> String {
        self.info().address
    }

    /// What the device can do, as the `a{sv}` of §3.
    #[zbus(property)]
    fn capabilities(&self) -> Dict {
        convert::capabilities(&self.info().capabilities)
    }

    /// The profiles this daemon will actually run for this scanner.
    #[zbus(property)]
    fn supported_profiles(&self) -> Vec<String> {
        self.info()
            .supported_profiles()
            .into_iter()
            .map(|kind| kind.as_str().to_owned())
            .collect()
    }

    /// Whether this scanner is paired with this host.
    ///
    /// Independent of [`Scanner1::status`] (§9): a paired scanner that is switched off
    /// is `Paired=true, Status="offline"`.
    #[zbus(property)]
    fn paired(&self) -> bool {
        self.is_paired()
    }

    /// Whether the host is listening for this scanner's events.
    ///
    /// Exactly "the registry holds a listener task for this scanner", which is why it is
    /// `true` after a successful `Pair()` without a `Connect()`: pairing ends by asking
    /// for the same task (see [`crate::listeners`]), and a scanner that is paired and
    /// reachable is one this daemon is ready to receive from. `Disconnect()` is how a
    /// client opts out of that.
    ///
    /// It also goes `false` on its own, together with `Status="error"`, when a listener
    /// could not be restarted within its budget — a scanner that will never deliver
    /// another press must not keep claiming it is connected.
    #[zbus(property)]
    fn connected(&self) -> bool {
        self.is_connected()
    }

    /// Reachability: `"offline"`, `"online"`, `"busy"`, `"error"`.
    ///
    /// Written by the discovery round, which is where a backend gets to say whether it
    /// can still see the device — see
    /// [`ScannerRegistry::mark_unseen_offline`](crate::scanners::ScannerRegistry::mark_unseen_offline).
    /// Independent of `Paired`, which is the point of §9's "reachable ≠ paired": a paired
    /// scanner that a probe stops finding goes `offline` and stays paired.
    #[zbus(property)]
    fn status(&self) -> String {
        self.info().status.as_str().to_owned()
    }

    /// The profile applied to data that arrives without one, `""` for none (§3).
    #[zbus(property)]
    fn default_profile(&self) -> String {
        ProfileKind::optional_as_str(self.default_profile_value()).to_owned()
    }

    /// Sets the fallback profile, refusing anything outside `SupportedProfiles`.
    ///
    /// Validated at write time rather than at scan time: a value accepted here and
    /// refused when a button is pressed turns a configuration mistake into a scan the
    /// user watches fail, hours later, with nothing on screen to connect the two.
    ///
    /// `""` clears it, which is the `Job1.Profile` spelling of "raw" (§4) and the only
    /// way back once one is set.
    ///
    /// # Errors
    ///
    /// `org.freedesktop.DBus.Error.InvalidArgs` for anything that is not `""` or a member
    /// of `SupportedProfiles`. §8's name for this is
    /// `org.scanbus.Error.UnsupportedProfile`, and a **property setter cannot answer with
    /// it**: zbus serves `org.freedesktop.DBus.Properties` itself and its `Set` returns
    /// [`zbus::fdo::Error`], so the reply's name comes from that closed set whatever a
    /// setter returns. See [`crate::dbus::error`]. The message names the profile and the
    /// list it had to be in, so a client can still tell this apart from a malformed call.
    #[zbus(property)]
    async fn set_default_profile(&self, value: String) -> zbus::fdo::Result<()> {
        let supported = self.info().supported_profiles();

        let profile = ProfileKind::parse_optional(&value).map_err(|_| {
            zbus::fdo::Error::InvalidArgs(unsupported_profile_message(&value, &supported))
        })?;

        if let Some(profile) = profile
            && !supported.contains(&profile)
        {
            return Err(zbus::fdo::Error::InvalidArgs(unsupported_profile_message(
                &value, &supported,
            )));
        }

        *self
            .default_profile
            .lock()
            .expect("default profile lock poisoned") = profile;

        if let Some(scanners) = self.scanners.upgrade() {
            scanners
                .persist_default_profile(&self.info().id, profile)
                .await;
        }

        debug!(id = %self.info().id, profile = %value, "default profile set");

        Ok(())
    }

    /// Where the pairing process is — `"none"`, `"pairing"`, `"installing_backend"`,
    /// `"done"`, `"failed"`.
    #[zbus(property)]
    fn pairing_state(&self) -> String {
        self.state().as_str().to_owned()
    }

    /// Failure detail for `PairingState="failed"`, empty otherwise.
    #[zbus(property)]
    fn pairing_error(&self) -> String {
        self.state().pairing_error().to_owned()
    }

    /// Starts pairing and returns — the asynchronous contract of §9.
    ///
    /// The reply says only that the process has started. `PairingState` is already
    /// `"pairing"` by the time it arrives, and the outcome reaches the client as
    /// `PropertiesChanged`; §7 of [`scanbus-cli.md`] spells out the "subscribe, call,
    /// read once" idiom that makes that raceless.
    ///
    /// A second `Pair()` on a scanner already pairing succeeds and changes nothing, per
    /// §9.
    ///
    /// # Errors
    ///
    /// `org.scanbus.Error.AlreadyPaired` when `Paired=true` — §9's other idempotency
    /// rule, and the case a client has to tell apart from "already in progress" because
    /// only one of them will ever produce a transition to wait for.
    /// `org.freedesktop.DBus.Error.InvalidArgs` for any key in `options`: §3 documents
    /// none, so a key here is a client expecting behaviour this daemon does not have.
    ///
    /// [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md
    #[instrument(level = "info", skip_all, fields(id = %self.info().id))]
    fn pair(&self, options: HashMap<String, OwnedValue>) -> Result<(), ScanbusError> {
        reject_unknown_options("Pair", &options, &[])?;

        match self.machine.pair() {
            PairOutcome::Started => {
                info!("pairing started");
                Ok(())
            }
            PairOutcome::AlreadyInProgress => {
                // Not an error: §9 says a second Pair() "simply returns with no effect".
                debug!("pairing is already in progress; leaving it alone");
                Ok(())
            }
            PairOutcome::AlreadyPaired => Err(ScanbusError::AlreadyPaired(format!(
                "scanner {} is already paired",
                self.info().id
            ))),
        }
    }

    /// Cancels a pairing in progress, resetting `PairingState` to `"none"` (§3).
    ///
    /// Succeeds when nothing is in progress: `CancelPairing()` on a `done` or `failed`
    /// scanner has nothing to cancel, and a client that lost the race with a transition
    /// should not have to tell that apart from a real failure.
    #[instrument(level = "info", skip_all, fields(id = %self.info().id))]
    fn cancel_pairing(&self) {
        let was = self.state();
        self.machine.cancel();

        if was.is_in_progress() {
            info!(from = %was, "pairing cancelled");
        } else {
            debug!(state = %was, "nothing to cancel");
        }
    }

    /// Declares the host ready to receive: starts consuming this scanner's button
    /// stream, and keeps consuming it until `Disconnect()`.
    ///
    /// Idempotent, because the task belongs to the registry and this only ensures it
    /// exists — a second `Connect()` finds the first one's listener and returns, which is
    /// also what a `Connect()` after a `Pair()` does.
    ///
    /// `options` may carry `{"profile": "document"}`, the session profile of §3. It is a
    /// per-connection override ranking *between* the button's own `Profile` and
    /// `DefaultProfile`; the order is written down in
    /// [`ButtonEvent::profile`](crate::listeners::ButtonEvent::profile) and read by 2.6.
    /// `Disconnect()` clears it.
    ///
    /// # Errors
    ///
    /// `org.scanbus.Error.NotPaired` on a scanner that has no association — connecting to
    /// a device found by discovery is meaningless, since nothing has configured its keys.
    /// `org.scanbus.Error.NotReachable` when `Status="offline"`, which is §3's own
    /// condition, and again when the backend cannot reach the device to arm it.
    /// `org.scanbus.Error.Busy` when something else already holds the device's event
    /// channel. `org.scanbus.Error.UnsupportedProfile` for a `profile` outside
    /// `SupportedProfiles` — a method, unlike the `DefaultProfile` setter, can answer with
    /// §8's name for this (see [`crate::dbus::error`]).
    /// `org.freedesktop.DBus.Error.InvalidArgs` for any other key in `options`.
    ///
    /// A refused call changes nothing, session profile included.
    #[instrument(level = "info", skip_all, fields(id = %self.info().id))]
    async fn connect(&self, options: HashMap<String, OwnedValue>) -> Result<(), ScanbusError> {
        let info = self.info();
        let profile = session_profile(&options, &info)?;

        if !self.is_paired() {
            return Err(ScanbusError::NotPaired(format!(
                "scanner {} is not paired; pair it before connecting",
                info.id
            )));
        }
        // §3's own precondition. Checked against the property a client can read, so a
        // refusal is one it could have predicted.
        if info.status == Status::Offline {
            return Err(ScanbusError::NotReachable(format!(
                "scanner {} is offline",
                info.id
            )));
        }

        let Some(scanners) = self.scanners.upgrade() else {
            return Err(ScanbusError::Failed(
                "the daemon is shutting down".to_owned(),
            ));
        };

        // Set before the listener starts, and restored if it does not: a press arriving
        // in the same instant has to be stamped with the profile this call asked for,
        // and a `Connect()` that failed must not have changed one.
        let previous = std::mem::replace(
            &mut *self
                .session_profile
                .lock()
                .expect("session profile lock poisoned"),
            profile,
        );

        match scanners.ensure_listening(&info.id).await {
            Ok(()) => {
                info!(profile = ProfileKind::optional_as_str(profile), "connected");
                Ok(())
            }
            Err(error) => {
                *self
                    .session_profile
                    .lock()
                    .expect("session profile lock poisoned") = previous;
                Err(connect_error(error))
            }
        }
    }

    /// Stops listening on the host side, and forgets the session profile.
    ///
    /// Succeeds when nothing is connected: §3 gives it no error, and a client that lost
    /// the race with a listener which had already given up
    /// (`Status="error"`) should not have to tell that apart from a real failure. The
    /// pairing is untouched — `Disconnect()` is not `Unpair()`, and a disconnected
    /// scanner keeps its keys, its mappings and its `Paired=true`.
    #[instrument(level = "info", skip_all, fields(id = %self.info().id))]
    async fn disconnect(&self) {
        let id = self.info().id;

        *self
            .session_profile
            .lock()
            .expect("session profile lock poisoned") = None;

        // Awaited, like `Unpair()`'s retire below: a client that calls `Disconnect()` and
        // then presses the key must not still be listened to. Safe to await from inside a
        // method handler only because every method here takes `&self` — see the module
        // documentation.
        match self.scanners.upgrade() {
            Some(scanners) => {
                if scanners.stop_listening(&id).await {
                    info!("disconnected");
                } else {
                    debug!("nothing was listening");
                }
            }
            None => debug!("disconnected during shutdown; nothing left to stop"),
        }
    }

    /// Removes the association, and with it the object — unless discovery still sees it.
    ///
    /// Two of the three things `Unpair()` undoes are done by [`PairingMachine::unpair`]:
    /// the persisted entry is removed ([4.1]) and `Paired` goes false. The third — the
    /// listener — is stopped *here*, before the machine runs, because the task consuming
    /// the stream is the registry's and not the machine's: letting the machine's own
    /// [`stop_listening`](scanbus_core::ScannerBackend::stop_listening) end a stream a
    /// live task is still reading would look to that task exactly like a device going
    /// away, and it would dutifully start reconnecting to a scanner that has just been
    /// unpaired. The machine's call then finds nothing left to stop, which the trait
    /// requires to be a no-op rather than an error.
    ///
    /// What is left after that is the object's lifetime, which is the registry's to
    /// decide (§1): a scanner the current discovery session has seen keeps its object
    /// with `Paired=false`, exactly as if it had just been discovered, and one that
    /// exists only because it was paired goes away with an `InterfacesRemoved`.
    ///
    /// # Errors
    ///
    /// `org.scanbus.Error.NotPaired` when there is no association to remove, which is
    /// what a second `Unpair()` gets. `org.freedesktop.DBus.Error.Failed` when the
    /// pairing could not be forgotten — the scanner stays paired in that case, because a
    /// `Paired=false` that a restart would undo is worse than a failure the user can
    /// retry.
    ///
    /// [4.1]: https://github.com/jeanparpaillon/scanbus_service/issues/16
    #[instrument(level = "info", skip_all, fields(id = %self.info().id))]
    async fn unpair(&self) -> Result<(), ScanbusError> {
        let id = self.info().id;
        let scanners = self.scanners.upgrade();

        // Before the machine, for the reason given above. `None` when the whole registry
        // is going away, which stops every listener anyway.
        let was_listening = match &scanners {
            Some(scanners) => scanners.stop_listening(&id).await,
            None => false,
        };
        *self
            .session_profile
            .lock()
            .expect("session profile lock poisoned") = None;

        match self.machine.unpair().await {
            Ok(None) => {}
            // The pairing is gone; the listener is the backend's problem now.
            Ok(Some(error)) => warn!(%error, "the listener did not stop, but the pairing is gone"),
            Err(UnpairError::NotPaired(id)) => {
                return Err(ScanbusError::NotPaired(format!(
                    "scanner {id} is not paired"
                )));
            }
            Err(error @ UnpairError::Store { .. }) => {
                // The scanner stays paired, so it must also stay connected: a failed
                // `Unpair()` the user can retry must not silently cost them the listener
                // that makes the scanner useful in the meantime.
                if was_listening
                    && let Some(scanners) = &scanners
                    && let Err(error) = scanners.ensure_listening(&id).await
                {
                    warn!(%error, "the unpairing failed and the listener could not be restored");
                }
                return Err(ScanbusError::Failed(error.to_string()));
            }
        }

        // After the machine, and awaited: a client that calls Unpair() and then
        // GetManagedObjects() must not still be shown an object the daemon has retired.
        // Safe to await from inside a method handler only because every method here takes
        // `&self` — see the module documentation.
        match scanners {
            Some(scanners) => match scanners.retire(&id).await {
                Retired::Removed => info!("unpaired; object removed"),
                Retired::Kept => info!("unpaired; object kept for the discovery session"),
                Retired::Unknown => warn!("unpaired a scanner the registry does not know"),
            },
            // The registry is gone, so the daemon is shutting down and the whole tree
            // with it. The unpairing itself still happened.
            None => debug!("unpaired during shutdown; nothing left to retire"),
        }

        Ok(())
    }
}

/// The message an unsupported `DefaultProfile` write comes back with.
fn unsupported_profile_message(value: &str, supported: &[ProfileKind]) -> String {
    let names: Vec<&str> = supported.iter().map(|kind| kind.as_str()).collect();
    format!(
        "profile {value:?} is not supported by this scanner; SupportedProfiles is \
         [{}] and \"\" clears the default",
        names.join(", ")
    )
}

/// Refuses any key of a `a{sv}` argument that is not in `known`.
///
/// §3 documents one option across these methods — `Connect`'s `profile` — so the honest
/// answer to anything else is that this daemon does not implement it. Accepting and
/// ignoring it would have a client believe a pairing hint took effect, and it is the same
/// reasoning that has `StartDiscovery` refuse a filter key it does not know
/// ([`crate::dbus::manager`]). Whatever adds the next real option ([9.3] wants a PIN)
/// declares it here.
///
/// [9.3]: https://github.com/jeanparpaillon/scanbus_service/issues/42
fn reject_unknown_options(
    method: &str,
    options: &HashMap<String, OwnedValue>,
    known: &[&str],
) -> Result<(), ScanbusError> {
    let Some(key) = options.keys().find(|key| !known.contains(&key.as_str())) else {
        return Ok(());
    };

    let understood = if known.is_empty() {
        "none".to_owned()
    } else {
        format!("[{}]", known.join(", "))
    };
    Err(ScanbusError::InvalidArgs(format!(
        "unknown {method} option {key:?}; this daemon understands {understood}"
    )))
}

/// Reads the session profile out of a `Connect()` `options` map.
///
/// Validated at connect time rather than at scan time, for the same reason the
/// `DefaultProfile` setter validates on write: a profile accepted here and refused when a
/// key is pressed turns a client's mistake into a scan the user watches fail, with
/// nothing on screen to connect the two.
///
/// `""` is `None` — §4's spelling of "deliver it raw" — so a client can clear the session
/// profile with a second `Connect()` instead of having to disconnect.
///
/// # Errors
///
/// `org.freedesktop.DBus.Error.InvalidArgs` for an unknown key or a `profile` that is not
/// a string; `org.scanbus.Error.UnsupportedProfile` for a name outside
/// `SupportedProfiles`, including one that is not a profile name at all — a client asking
/// for `"pdf"` and a client asking for `"ocr"` are both asking for something this scanner
/// will not run, and §8 has one name for that.
fn session_profile(
    options: &HashMap<String, OwnedValue>,
    info: &ScannerInfo,
) -> Result<Option<ProfileKind>, ScanbusError> {
    reject_unknown_options("Connect", options, &[PROFILE_OPTION])?;

    let Some(value) = options.get(PROFILE_OPTION) else {
        return Ok(None);
    };

    let name = String::try_from(value.clone()).map_err(|_| {
        ScanbusError::InvalidArgs(format!(
            "the {PROFILE_OPTION:?} option must be a string, got signature {}",
            value.value_signature()
        ))
    })?;

    let supported = info.supported_profiles();
    let profile = ProfileKind::parse_optional(&name).map_err(|_| {
        ScanbusError::UnsupportedProfile(unsupported_profile_message(&name, &supported))
    })?;

    if let Some(profile) = profile
        && !supported.contains(&profile)
    {
        return Err(ScanbusError::UnsupportedProfile(
            unsupported_profile_message(&name, &supported),
        ));
    }

    Ok(profile)
}

/// The D-Bus error a `Connect()` refusal comes back with.
///
/// Backend failures go through [2.7]'s total `BackendError` mapping in
/// [`crate::dbus::error`]. The extra case is `ListenError::Unknown`, where the scanner
/// object has gone between lookup and call.
///
/// [2.7]: https://github.com/jeanparpaillon/scanbus_service/issues/11
fn connect_error(error: ListenError) -> ScanbusError {
    match error {
        ListenError::Unknown(id) => {
            ScanbusError::UnknownObject(format!("scanner {id} no longer has an object"))
        }
        ListenError::Backend(error) => error.into(),
    }
}

/// Sets `Connected` on an exported object, emitting `PropertiesChanged` if it moved.
///
/// The registry's writer for that property, and the only one: `Connected` means "there is
/// a listener task", the registry is what owns those tasks, and a second writer is how a
/// property ends up disagreeing with the thing it describes.
///
/// # Errors
///
/// Whatever zbus failed to emit. A failure leaves the in-memory value updated and the
/// client's copy stale, which is why the caller logs it rather than swallowing it.
pub async fn set_connected(iface: &InterfaceRef<Scanner1>, connected: bool) -> zbus::Result<()> {
    // `get`, never `get_mut`: see the module documentation on why this interface is only
    // ever read-locked.
    let scanner = iface.get().await;

    {
        let mut current = scanner.connected.lock().expect("connected lock poisoned");
        if *current == connected {
            return Ok(());
        }
        *current = connected;
    }

    debug!(id = %scanner.info().id, connected, "Connected changed");
    scanner.connected_changed(iface.signal_emitter()).await
}

/// Applies a fresh [`ScannerInfo`] to an exported object, emitting `PropertiesChanged`
/// for exactly the properties that moved.
///
/// This is what "a rediscovered scanner updates the existing object rather than adding a
/// second one" means in practice (§1): the identity is [`ScannerInfo::id`], and
/// everything else — a device renamed on its front panel, an address that moved with
/// DHCP, a scanner that came back online — is a property change on the object a client
/// is already watching.
///
/// `Id` is deliberately not in the diff: it is the path element, so a change in it is a
/// different object, and the registry never routes one here. `Paired` and `PairingState`
/// are not in it either — they are the host's half of the state, and a backend
/// rediscovering a scanner must not be able to reset a pairing.
///
/// # Errors
///
/// Whatever zbus failed to emit. A failure here leaves the in-memory value updated and
/// the client's copy stale, which is why the caller logs it rather than swallowing it.
pub async fn update(iface: &InterfaceRef<Scanner1>, info: &ScannerInfo) -> zbus::Result<()> {
    // `get`, never `get_mut`: see the module documentation on why this interface is only
    // ever read-locked. The mutation happens inside the machine's own lock.
    let scanner = iface.get().await;
    let previous = scanner.machine.set_scanner(info.clone());
    let emitter = iface.signal_emitter();

    if previous.name != info.name {
        scanner.name_changed(emitter).await?;
    }
    if previous.backend != info.backend {
        scanner.backend_changed(emitter).await?;
    }
    if previous.address != info.address {
        scanner.address_changed(emitter).await?;
    }
    if previous.capabilities != info.capabilities {
        scanner.capabilities_changed(emitter).await?;
        // `SupportedProfiles` is derived from the info, so it is checked here rather
        // than compared: the derivation is the same one the getter runs.
        if previous.supported_profiles() != info.supported_profiles() {
            scanner.supported_profiles_changed(emitter).await?;
        }
    }
    if previous.status != info.status {
        debug!(id = %info.id, from = %previous.status, to = %info.status, "status changed");
        scanner.status_changed(emitter).await?;
    }

    Ok(())
}

/// Turns one scanner's pairing transitions into `PropertiesChanged`, for as long as the
/// object exists.
///
/// The single place the checklist of 2.3 asks for. It is a task rather than a call
/// because the transitions come from a pairing that outlives the `Pair()` that started
/// it, and it holds no [`InterfaceRef`] between transitions — that would keep the
/// interface, the machine and this very receiver alive after the object was unexported,
/// and the task would never end. Looking the object up per transition is also how it
/// learns the object is gone: [`ObjectRegistry::interface`] failing is the exit
/// condition.
///
/// `initial` is where the machine stood when the caller subscribed. The channel carries
/// no starting value, so this is what the first transition is compared against —
/// `"none"` for a discovered scanner, `"done"` for one restored from the store.
pub async fn supervise(
    objects: Arc<ObjectRegistry>,
    scanners: Weak<ScannerRegistry>,
    path: OwnedObjectPath,
    id: ScannerId,
    initial: PairingState,
    mut transitions: broadcast::Receiver<PairingState>,
) {
    let mut paired = initial == PairingState::Done;
    let mut previous = initial;

    loop {
        let state = match transitions.recv().await {
            Ok(state) => state,
            // The machine went with the object; there is nothing left to announce.
            Err(broadcast::error::RecvError::Closed) => break,
            // Only reachable if this task stalled for a whole buffer's worth of
            // transitions. Skipping to wherever the machine is now is the honest
            // recovery: the property's job is to end up right, not to replay a history
            // the client has already missed.
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                warn!(%path, missed, "pairing transitions were dropped; resyncing");
                match objects.interface::<Scanner1>(&path).await {
                    Ok(iface) => iface.get().await.state(),
                    Err(_) => break,
                }
            }
        };

        // The machine relays every `PairingProgress`, and `Checking` maps to the
        // `Pairing` it is already in. A `PropertiesChanged` carrying the value the client
        // already has is noise.
        if state == previous {
            continue;
        }

        let now_paired = state == PairingState::Done;

        // Before the signal, not after: a client that reacts to `PairingState="done"` by
        // calling `StopDiscovery` must not be able to take the object away between the
        // two, and only the promotion stops that.
        if now_paired && let Some(scanners) = scanners.upgrade() {
            scanners.promote(&id).await;
        }

        let Ok(iface) = objects.interface::<Scanner1>(&path).await else {
            debug!(%path, "the scanner object is gone; nothing left to announce");
            break;
        };

        if let Err(error) = emit(&iface, &previous, &state, &mut paired).await {
            warn!(%path, %error, "could not announce a pairing transition");
        }

        // *After* the signal, unlike the promotion: this emits `Connected=true`, and a
        // client should learn that a scanner paired before it learns the daemon is
        // already listening to it.
        //
        // 1.4 opens a stream as its last step and drops it again precisely so that this
        // is the only owner ([`crate::listeners`]). A failure is logged and not fatal —
        // the scanner is paired, `Connected` stays `false`, and a `Connect()` gets the
        // client a real error to act on.
        if now_paired
            && let Some(scanners) = scanners.upgrade()
            && let Err(error) = scanners.ensure_listening(&id).await
        {
            warn!(%path, %error, "paired, but the button listener could not be started");
        }

        previous = state;
    }

    debug!(%path, "pairing supervisor finished");
}

/// Emits the `PropertiesChanged` for one transition, and nothing that did not move.
///
/// The values come from the transition rather than from the getters, which is the whole
/// reason the machine's channel is lossless: a getter reads wherever the machine is
/// *now*, so a signal built from one would report `"done"` for the
/// `"installing_backend"` it was sent for, and a client following the signals would never
/// see the state §7's flow promises it.
async fn emit(
    iface: &InterfaceRef<Scanner1>,
    previous: &PairingState,
    state: &PairingState,
    paired: &mut bool,
) -> zbus::Result<()> {
    let mut changed: HashMap<&str, Value<'_>> = HashMap::new();

    if previous.as_str() != state.as_str() {
        debug!(from = %previous, to = %state, "pairing state changed");
        changed.insert("PairingState", Value::from(state.as_str()));
    }
    // Its own comparison, because `failed → failed` with a different message is a real
    // change to this property and `pairing → done` is not one.
    if previous.pairing_error() != state.pairing_error() {
        changed.insert("PairingError", Value::from(state.pairing_error()));
    }

    // Derived from the transition, not read back off the machine: an `Unpair()` landing
    // between the `Done` transition and this would otherwise make both signals agree
    // that nothing about `Paired` had moved.
    let now = match state {
        PairingState::Done => Some(true),
        PairingState::None => Some(false),
        PairingState::Pairing | PairingState::InstallingBackend | PairingState::Failed(_) => None,
    };
    if let Some(now) = now
        && now != *paired
    {
        *paired = now;
        changed.insert("Paired", Value::from(now));
    }

    if changed.is_empty() {
        return Ok(());
    }

    zbus::fdo::Properties::properties_changed(
        iface.signal_emitter(),
        InterfaceName::from_static_str_unchecked(INTERFACE),
        changed,
        Cow::Borrowed(&[]),
    )
    .await
}
