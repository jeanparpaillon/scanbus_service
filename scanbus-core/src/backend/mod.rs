//! The [`ScannerBackend`] seam.
//!
//! This is the abstraction point that lets Brother and HP be added without touching
//! D-Bus code ([`scanbus-rust-implementation.md`] §3). Everything above it — the
//! registry, the pairing state machine, the profile pipeline — is written against the
//! trait and tested against [`mock::MockBackend`]; everything below it shells out to
//! vendor tooling that no CI runner has.
//!
//! Two shape decisions are worth stating here, because both are cheap now and expensive
//! once two backends implement the trait:
//!
//! - [`start_listening`](ScannerBackend::start_listening) returns a **stream**, not a
//!   callback. The daemon owns the task that consumes it, stops listening by dropping
//!   it, and holds nothing of the backend's while a job runs.
//! - [`ensure_installed`](ScannerBackend::ensure_installed) takes an
//!   [`mpsc::Sender`] rather than returning progress. `Pair()` returns immediately and
//!   the install takes tens of seconds ([`scanbus-dbus-api.md`] §9), so progress has to
//!   reach the state machine while the call is still running.
//!
//! [`scanbus-rust-implementation.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-rust-implementation.md
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md

use std::collections::BTreeMap;

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use tokio::sync::mpsc;

use crate::error::BackendError;
use crate::model::{ProfileKind, RawPage, ScannerId, ScannerInfo, Value};

mod event;

// Compiled for the crate's own tests unconditionally, and for downstream crates behind
// the `mock` feature — 2.8 and every later acceptance test drive the D-Bus surface
// through it. `cargo doc --all-features` is what shows it.
#[cfg(any(feature = "mock", test))]
pub mod mock;

pub use event::{PairingProgress, ScanTrigger, TriggerId, TriggerKind};

/// What a backend says about a daemon-store pairing during startup reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreDisposition {
    /// The backend still has its own half of the pairing.
    Paired,
    /// The backend is missing what it needs; the object stays present but unpaired.
    Failed(String),
}

/// One way of finding and driving scanners: SANE, eSCL, `brscan-skey`, HPLIP.
///
/// Implementors are held as `Arc<dyn ScannerBackend>` and shared across tasks, hence
/// `Send + Sync` and `&self` everywhere: a backend's interior state (a spawned
/// `brscan-skey`, an open hpmud channel) is its own problem, behind its own lock.
#[async_trait]
pub trait ScannerBackend: Send + Sync {
    /// Backend identifier, e.g. `"hplip"`, `"brother-skey"`.
    ///
    /// Distinct from `ScannerInfo::backend`, which is the free-form `Backend` property
    /// a client sees, and from the `backend` argument of
    /// [`ScannerId::from_backend`](crate::model::ScannerId::from_backend), which must
    /// match `[a-z0-9]` because it is part of an object path.
    fn id(&self) -> &'static str;

    /// Network/USB discovery specific to this backend.
    ///
    /// Called on every `StartDiscovery()`, so it must be safe to run repeatedly and
    /// while scanners are paired and listening. Returning a scanner again is normal and
    /// must not disturb it: the daemon merges by [`ScannerInfo::id`] and keeps its own
    /// half of the state (`Paired`, `PairingState`, the button assignments).
    ///
    /// # Errors
    ///
    /// [`BackendError::NotReachable`] when the probe itself could not run — no daemon
    /// to ask, no network. A probe that simply finds nothing returns an empty `Vec`.
    async fn discover(&self) -> Result<Vec<ScannerInfo>, BackendError>;

    /// Installs or checks the system dependencies this scanner needs — packages, udev
    /// rules, vendor daemons.
    ///
    /// Emits [`PairingProgress`] through `progress` as it goes, ending in exactly one
    /// of [`PairingProgress::Ready`] or [`PairingProgress::Failed`]; the failure step
    /// must be sent *before* the `Err` is returned, so that a client watching
    /// `PropertiesChanged` and a caller reading the `Result` never disagree. A send
    /// that fails because the receiver was dropped is not itself an error — the daemon
    /// may have stopped watching — and must not abort the install.
    ///
    /// Must be idempotent: called on a system where everything is already present it
    /// reports [`PairingProgress::Ready`] and returns `Ok` without touching anything.
    ///
    /// # Cancellation
    ///
    /// The daemon cancels `CancelPairing()` by **dropping this future**, possibly
    /// mid-install. Every implementor therefore has to leave the system in a state the
    /// next call can recover from: commit installed-ness only once it is true, unwind
    /// or leave alone anything half-written, and never record success at a point the
    /// drop can happen after. The test for it is that a dropped call followed by a
    /// fresh one succeeds and reports the same progress sequence as a first run.
    ///
    /// # Errors
    ///
    /// [`BackendError::InstallFailed`] with the package that failed;
    /// [`BackendError::NotReachable`] if the device has to be reachable to be probed
    /// and is not.
    async fn ensure_installed(
        &self,
        scanner: &ScannerInfo,
        progress: mpsc::Sender<PairingProgress>,
    ) -> Result<(), BackendError>;

    /// Starts listening for physical events on this scanner.
    ///
    /// The returned stream yields a [`ScanTrigger`] per trigger and ends when the
    /// backend stops producing — the device went away, the vendor daemon died. The
    /// caller stops listening either by dropping the stream or by calling
    /// [`stop_listening`](ScannerBackend::stop_listening); both are legal, and doing
    /// both in either order must work.
    ///
    /// # Errors
    ///
    /// [`BackendError::Busy`] if a listener for this scanner is already running,
    /// [`BackendError::NotReachable`] if the device cannot be reached to be armed.
    async fn start_listening(
        &self,
        scanner: &ScannerInfo,
    ) -> Result<BoxStream<'static, ScanTrigger>, BackendError>;

    /// Stops the listener started by
    /// [`start_listening`](ScannerBackend::start_listening).
    ///
    /// **Idempotent, and a no-op rather than an error** when nothing is listening —
    /// including when the caller has already dropped the stream, which is precisely
    /// what a `Disconnect()` after a crashed consumer task looks like. Reserve errors
    /// for a backend that genuinely could not tear its listener down.
    ///
    /// # Errors
    ///
    /// [`BackendError::Other`] when the teardown itself failed, e.g. a vendor daemon
    /// that refused to stop.
    async fn stop_listening(&self, scanner_id: &ScannerId) -> Result<(), BackendError>;

    /// Reconciles one daemon-store pairing with the backend's own durable state.
    ///
    /// Default `Paired`: backends that keep no private pairing state have nothing to
    /// disagree with. A backend that does keep some — a token, an app credential —
    /// returns [`RestoreDisposition::Failed`] when the daemon says paired and it does
    /// not, which is what lets startup surface "pair again" instead of reviving a
    /// scanner that can never work.
    async fn restore_disposition(&self, _scanner: &ScannerInfo) -> RestoreDisposition {
        RestoreDisposition::Paired
    }

    /// Drops backend-side pairings for scanners the daemon store no longer names.
    ///
    /// Default no-op for the same reason as [`ScannerBackend::restore_disposition`]:
    /// backends with no private durable state have nothing to prune.
    async fn prune_unrestored_pairings(&self, _restored: &[ScannerId]) -> Result<(), BackendError> {
        Ok(())
    }

    /// Revokes whatever backend-side state pairing created — a token, a
    /// `brscan-skey.config` entry — so the device is no longer recognised.
    ///
    /// Called by `Unpair()` (§3) before the pairing store entry is dropped. Distinct
    /// from [`stop_listening`](ScannerBackend::stop_listening): a paired device whose
    /// listener has stopped must still be recognised when it reconnects, but a
    /// *forgotten* one must not be. The default no-op is correct for a backend with no
    /// such state to revoke, e.g. one that only ever identifies a device by its
    /// physical address.
    ///
    /// # Errors
    ///
    /// [`BackendError::Other`] when the revocation itself failed. `Unpair()` reports
    /// this without leaving the scanner paired: forgetting the pairing store entry
    /// still goes ahead, on the same reasoning as a listener that will not stop.
    async fn forget(&self, _scanner_id: &ScannerId) -> Result<(), BackendError> {
        Ok(())
    }

    /// Writes the key→profile mapping into the backend's own configuration, e.g.
    /// `brscan-skey.conf`, and reloads it.
    ///
    /// `profile` is an `Option` where §3 of the implementation plan wrote a bare
    /// `ProfileKind`, because `Button1.Profile` is writable and `""` means unassigned
    /// (§5 of the API): clearing a key has to reach the vendor config too, or the
    /// device keeps triggering a scan the daemon no longer has a mapping for. `None` is
    /// therefore "remove this key's entry", not "leave it as it is".
    ///
    /// `options` is borrowed, and a [`BTreeMap`] rather than the `HashMap` of §3, to
    /// match [`ButtonInfo::profile_options`](crate::model::ButtonInfo::profile_options)
    /// — the map this is always called with — and to keep the written config's key
    /// order stable, which matters for a file that a user may also read.
    ///
    /// # Errors
    ///
    /// [`BackendError::UnsupportedProfile`] when the device offers nothing this profile
    /// can be mapped onto, [`BackendError::UnknownScanner`] for a scanner the backend
    /// has no configuration for.
    ///
    /// A backend that has keys, handed a `button_index` that is not one of them, answers
    /// [`BackendError::Other`] carrying what the device *does* offer — HPLIP's single
    /// walk-up trigger, Brother's four panel entries — and must change nothing, neither
    /// the device nor any record of what a key means. (A backend with no keys at all is
    /// the other case and says [`BackendError::Unsupported`] for every index: the mobile
    /// backend has no panel to be wrong about.)
    ///
    /// `Other` rather than one of §8's named errors because none of them is true — the
    /// scanner is known, the operation is supported, the device has said nothing. It maps
    /// to `org.freedesktop.DBus.Error.Failed`, and that is the right answer for a
    /// condition no client can provoke: `Button1` objects are exported from
    /// `Capabilities.buttons.count`, so the range a client can address is the range the
    /// backend has. What can produce a bad index is our own code — a restore replaying an
    /// assignment older than the key set (4.2), a backend whose count and whose table
    /// have drifted — and those are bugs to surface, not to half-apply. Clearing is no
    /// exception: `None` for an index that does not exist is not "nothing to remove", it
    /// is the same bug arriving by the other setter.
    async fn set_button_mapping(
        &self,
        scanner_id: &ScannerId,
        button_index: u32,
        profile: Option<ProfileKind>,
        options: &BTreeMap<String, Value>,
    ) -> Result<(), BackendError>;

    /// Fetches the raw data of a scan the device has already started.
    ///
    /// **Callable exactly once per `trigger_id`.** The pages are a one-shot transfer, not a
    /// buffer the backend keeps: a second call for the same id gets
    /// [`BackendError::UnknownJob`], the same as an id that never existed. A daemon
    /// that needs the pages twice keeps them itself — which is what the document
    /// profile does when it buffers an ADF batch for PDF assembly (§6).
    ///
    /// `trigger_id` is the backend's, minted when the trigger arrived and handed back by
    /// the daemon unchanged. Its string form is what a vendor tool ends up keying its
    /// spool directory on (5.3).
    ///
    /// The stream ends when the device reports no further page; for a flatbed that is
    /// after one, for an ADF batch after the last sheet. That end is what moves `Job1`
    /// from `"receiving"` to `"processing"` (§9).
    ///
    /// # A stream that fails is not a stream that ended
    ///
    /// The items are `Result`s, and that is load-bearing: "the ADF ran out of sheets" and
    /// "the device stopped answering after page 3" are the same event to a consumer of a
    /// bare `Stream<Item = RawPage>`, and `Job1` has to tell them apart — one lands the
    /// job in `"done"` and the other in `"error"` with a message (§4). A backend reports
    /// the second by yielding one `Err` and then ending; nothing is expected after it.
    ///
    /// # Errors
    ///
    /// [`BackendError::UnknownJob`] for an unknown or already-fetched job, and
    /// [`BackendError::NotReachable`] if the device was already gone when the transfer
    /// was asked for. A device that goes away *during* the transfer is the `Err` item
    /// above, not this.
    async fn fetch_pages(
        &self,
        scanner_id: &ScannerId,
        trigger_id: &str,
    ) -> Result<BoxStream<'static, Result<RawPage, BackendError>>, BackendError>;
}
