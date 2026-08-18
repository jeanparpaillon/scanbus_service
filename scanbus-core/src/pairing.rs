//! [`PairingMachine`]: the `PairingState` state machine on the daemon side (§5),
//! driven by a `&dyn ScannerBackend`, with no D-Bus in sight.
//!
//! The sequence per §5 is `Pairing` → `ensure_installed` → `InstallingBackend` →
//! `start_listening` → `Done`/`Failed`. The daemon's job ([`2.3`]) is only to turn the
//! transitions this emits into `PropertiesChanged`; everything about *when* the state
//! changes, and the awkward lifetime cases — a cancel racing the transition to `done`,
//! two concurrent `Pair()` calls, a `Failed` scanner staying retryable — is settled
//! here, against [`mock::MockBackend`](crate::backend::mock::MockBackend), with no bus
//! and no printer.
//!
//! `CancelPairing()` (§5) is an [`AbortHandle`] on the task driving the current step:
//! [`PairingMachine::cancel`] aborts it and lands the state on
//! [`PairingState::None`](crate::model::PairingState::None), so a retry is a plain
//! [`PairingMachine::pair`] rather than a special case.
//!
//! [`2.3`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/todo/2_3.md

use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use tokio::task::AbortHandle;

use crate::backend::{PairingProgress, ScannerBackend};
use crate::error::BackendError;
use crate::model::{ButtonInfo, PairingState, ProfileKind, ScannerId, ScannerInfo};

/// How many [`PairingProgress`] steps [`ScannerBackend::ensure_installed`] may have
/// in flight before the relay that turns them into transitions has to catch up.
///
/// Generous on purpose: a slow relay must never be the reason a backend's `.send`
/// blocks, since that `.send` failing silently (receiver dropped) is already part of
/// the trait's contract, but *blocking* it is not.
const PROGRESS_BUFFER: usize = 16;

/// How many transitions a subscriber may fall behind before it starts missing them.
///
/// One run of §5's sequence produces four at most, so this is several pairings' worth of
/// slack for a consumer that has stalled — and the consumer that matters, the daemon's
/// `PropertiesChanged` emitter, does one D-Bus signal per value. A [`broadcast`] channel
/// rather than a `watch` precisely because the intermediate values are the point:
/// `watch` keeps only the newest, and a scanner whose install starts a millisecond after
/// `Pair()` returns would reach a client as `pairing → done` with the
/// `"installing_backend"` the API promises never sent.
const TRANSITION_BUFFER: usize = 32;

/// Where a successfully paired scanner is made durable, before `Done` is announced.
///
/// Implemented against real storage in [`4.1`]; here it is the seam that lets a test
/// assert the write happens *before* the transition, not after — the property that
/// makes `Paired=true` survive a crash between the two.
///
/// [`4.1`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/todo/4_1.md
#[async_trait]
pub trait PairingStore: Send + Sync {
    /// Records that `scanner` has paired.
    ///
    /// Awaited before [`PairingState::Done`] is emitted: an error here must leave the
    /// machine on [`PairingState::Failed`] rather than announce a pairing that a
    /// restart would forget.
    ///
    /// # Errors
    ///
    /// Whatever the concrete store failed to do — write a file, commit a transaction —
    /// rendered as a message fit for [`PairingState::Failed`].
    async fn save_paired(&self, scanner: &ScannerInfo) -> Result<(), PairingStoreError>;

    /// Removes what [`PairingStore::save_paired`] recorded — `Unpair()` (§3).
    ///
    /// **Idempotent**: a scanner the store has never heard of is `Ok(())`, not an error.
    /// `Unpair()` is reachable on a scanner paired in an earlier run and already forgotten
    /// by hand, and a client retrying a call whose reply it lost must not get a failure
    /// the second time.
    ///
    /// Awaited *before* [`PairingMachine::unpair`] clears `Paired`, for the mirror image
    /// of the reason [`PairingStore::save_paired`] runs before `Done`: a failure here has
    /// to leave the scanner paired, or a restart would bring back a pairing the daemon
    /// has already told its clients is gone.
    ///
    /// # Errors
    ///
    /// Whatever the concrete store failed to do.
    async fn forget(&self, scanner_id: &ScannerId) -> Result<(), PairingStoreError>;

    /// Records `DefaultProfile` for a paired scanner.
    ///
    /// Default no-op so existing stores keep the 1.4 contract without having to
    /// persist scanner-side settings yet.
    async fn save_default_profile(
        &self,
        _scanner_id: &ScannerId,
        _profile: Option<ProfileKind>,
    ) -> Result<(), PairingStoreError> {
        Ok(())
    }

    /// Records one button assignment (`Label`, `Profile`, `ProfileOptions`).
    ///
    /// Default no-op for the same reason as
    /// [`PairingStore::save_default_profile`].
    async fn save_button(
        &self,
        _scanner_id: &ScannerId,
        _button: &ButtonInfo,
    ) -> Result<(), PairingStoreError> {
        Ok(())
    }

    /// Records whether a paired scanner has a listener running — the `Connected` half
    /// of what [`4.2`] restores.
    ///
    /// Default no-op for the same reason as [`PairingStore::save_default_profile`]: a
    /// store that does not implement restoration keeps working, it simply has nothing
    /// for [`PairingStore::restorable`] to report back.
    ///
    /// [`4.2`]: https://github.com/jeanparpaillon/scanbus_service/issues/17
    async fn save_connected(
        &self,
        _scanner_id: &ScannerId,
        _connected: bool,
    ) -> Result<(), PairingStoreError> {
        Ok(())
    }

    /// Every paired scanner this store knows of, for the restore path at startup
    /// ([`4.2`]).
    ///
    /// Default empty, matching the other defaults: a store with nothing durable behind
    /// it restores nothing, rather than the daemon failing to start.
    ///
    /// [`4.2`]: https://github.com/jeanparpaillon/scanbus_service/issues/17
    async fn restorable(&self) -> Result<Vec<Restorable>, PairingStoreError> {
        Ok(Vec::new())
    }
}

/// One paired scanner, as the restore path at startup ([`4.2`]) needs it: enough to
/// re-publish the object and decide whether to restart its listener.
///
/// [`4.2`]: https://github.com/jeanparpaillon/scanbus_service/issues/17
#[derive(Debug, Clone, PartialEq)]
pub struct Restorable {
    /// The scanner, as it was last known.
    pub scanner: ScannerInfo,
    /// Whether a listener was running for it when the daemon last recorded `Connected`.
    pub connected: bool,
    /// The host-owned half of each key (§5) as it was last persisted — `Label`,
    /// `Profile` and `ProfileOptions` — in index order.
    ///
    /// Only keys something was written to are in here: a store records a [`ButtonInfo`]
    /// when one of the three is set, never a default row per index. An empty vector is
    /// therefore a scanner nobody configured, and a store that keeps no assignments
    /// restores none — the same "nothing durable behind it" this trait's defaults allow
    /// for [`connected`](Self::connected).
    ///
    /// The *device's* half is deliberately not taken from here. `index`, `device_label`
    /// and `label_configurable` are what the backend reports now; the persisted copy
    /// describes the firmware as it was at the last write, and replaying it wholesale
    /// would let a stale label overwrite one a re-probe has since corrected.
    pub buttons: Vec<ButtonInfo>,
}

/// A [`PairingStore::save_paired`] failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{0}")]
pub struct PairingStoreError(pub String);

impl PairingStoreError {
    /// Wraps any displayable failure as a [`PairingStoreError`].
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// What [`PairingMachine::pair`] resulted in.
///
/// The two non-obvious outcomes are §9's idempotency rules: `Pair()` on a scanner that
/// is already mid-pairing does not restart it, and `Pair()` on one already paired is a
/// distinct, explicit case rather than being folded into "already in progress".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairOutcome {
    /// A fresh pairing attempt was started.
    Started,
    /// A pairing attempt was already running; this call left it alone.
    AlreadyInProgress,
    /// The scanner is already paired.
    AlreadyPaired,
}

/// What [`PairingMachine::unpair`] could not do.
///
/// Deliberately not "everything that can go wrong while unpairing": a backend that
/// cannot stop its listener is *not* in here, because [`PairingMachine::unpair`] does
/// not fail on it — see that method. What is left are the two conditions that must stop
/// the pairing from being forgotten.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UnpairError {
    /// The scanner is not paired, so there is no association to remove.
    #[error("scanner {0} is not paired")]
    NotPaired(ScannerId),

    /// The store refused to forget the pairing.
    ///
    /// The scanner stays paired: a `Paired=false` that a restart would undo is worse
    /// than a failed `Unpair()` the user can retry.
    #[error("the pairing of {scanner} could not be forgotten: {source}")]
    Store {
        /// The scanner that stays paired.
        scanner: ScannerId,
        /// What the store reported.
        #[source]
        source: PairingStoreError,
    },
}

/// Shared, lockable state — everything [`PairingMachine::pair`] and
/// [`PairingMachine::cancel`] have to agree on atomically.
struct Inner {
    /// The backend's last word on this scanner, as
    /// [`PairingMachine::set_scanner`] left it.
    scanner: ScannerInfo,
    state: PairingState,
    paired: bool,
    task: Option<AbortHandle>,
}

/// Drives one scanner's `PairingState` through §5.
///
/// Owns no bus, no timer: just the current [`PairingState`], a handle to the task
/// running the sequence, and what it takes to run one — a backend and a store.
///
/// The [`ScannerInfo`] is in here rather than beside it because there is exactly one
/// thing a stale copy of it can do: send `ensure_installed` at an address the device
/// moved off. [`PairingMachine::set_scanner`] is what a rediscovery calls, and the
/// daemon's `Scanner1` object reads its properties back out of here rather than keeping
/// a second copy that could disagree.
pub struct PairingMachine {
    backend: Arc<dyn ScannerBackend>,
    store: Arc<dyn PairingStore>,
    inner: Arc<Mutex<Inner>>,
    transitions: broadcast::Sender<PairingState>,
}

impl PairingMachine {
    /// A machine for `scanner`, starting at [`PairingState::None`] and unpaired.
    pub fn new(
        scanner: ScannerInfo,
        backend: Arc<dyn ScannerBackend>,
        store: Arc<dyn PairingStore>,
    ) -> Self {
        let (transitions, _) = broadcast::channel(TRANSITION_BUFFER);
        Self {
            backend,
            store,
            inner: Arc::new(Mutex::new(Inner {
                scanner,
                state: PairingState::None,
                paired: false,
                task: None,
            })),
            transitions,
        }
    }

    /// The scanner this machine is pairing.
    pub fn scanner(&self) -> ScannerInfo {
        self.lock().scanner.clone()
    }

    /// Replaces what the backend last reported, returning the previous value.
    ///
    /// Called on every rediscovery. It does not touch the pairing state: a scanner that
    /// was renamed or moved to another address mid-pairing is still the same scanner
    /// (§1 keys on [`ScannerInfo::id`]), and a pairing already in flight keeps the
    /// snapshot [`PairingMachine::pair`] took — restarting it under a running install is
    /// exactly what §9's idempotency rule forbids.
    pub fn set_scanner(&self, scanner: ScannerInfo) -> ScannerInfo {
        std::mem::replace(&mut self.lock().scanner, scanner)
    }

    /// The current `PairingState`.
    pub fn state(&self) -> PairingState {
        self.lock().state.clone()
    }

    /// The current `Paired` property — independent of `state()`, per §3: a `Failed`
    /// scanner from a *second* attempt is still `paired` if a first one already
    /// succeeded, since re-pairing an already-paired scanner is refused before it gets
    /// this far (see [`PairOutcome::AlreadyPaired`]).
    pub fn is_paired(&self) -> bool {
        self.lock().paired
    }

    /// A receiver of every `PairingState` this machine moves to from now on.
    ///
    /// This is the "channel" the module doc promises: the daemon (2.3) turns each
    /// value out of it into a `PropertiesChanged` for `PairingState`/`PairingError`.
    ///
    /// It carries no starting value — subscribe first, then read
    /// [`PairingMachine::state`], which is the same "subscribe before you call" order a
    /// D-Bus client uses for the properties this feeds.
    pub fn subscribe(&self) -> broadcast::Receiver<PairingState> {
        self.transitions.subscribe()
    }

    /// Starts pairing, unless one is already running or the scanner is already paired.
    ///
    /// Synchronous: the decision of whether to start is made and acted on under one
    /// lock, which is what makes two callers racing this method start the sequence
    /// exactly once (§9) rather than a `TOCTOU` window between checking the state and
    /// spawning the task.
    pub fn pair(&self) -> PairOutcome {
        let mut inner = self.lock();

        if inner.paired {
            return PairOutcome::AlreadyPaired;
        }
        if inner.state.is_in_progress() {
            return PairOutcome::AlreadyInProgress;
        }

        inner.state = PairingState::Pairing;
        let _ = self.transitions.send(PairingState::Pairing);

        let scanner = inner.scanner.clone();
        let backend = Arc::clone(&self.backend);
        let store = Arc::clone(&self.store);
        let task_inner = Arc::clone(&self.inner);
        let transitions = self.transitions.clone();

        let handle = tokio::spawn(async move {
            run(scanner, backend, store, task_inner, transitions).await;
        });
        inner.task = Some(handle.abort_handle());

        PairOutcome::Started
    }

    /// Aborts the in-flight step, if any, and lands on [`PairingState::None`].
    ///
    /// A no-op when nothing is in progress — a `Failed` or `Done` scanner has no task
    /// to abort, and cancelling either would be nonsense (`CancelPairing()` on a
    /// finished pairing has nothing to cancel).
    pub fn cancel(&self) {
        let mut inner = self.lock();

        let Some(task) = inner.task.take() else {
            return;
        };
        task.abort();

        if inner.state.is_in_progress() {
            inner.state = PairingState::None;
            let _ = self.transitions.send(PairingState::None);
        }
    }

    /// Undoes a pairing: stops the listener, forgets the persisted entry, and lands on
    /// [`PairingState::None`] with `Paired=false`.
    ///
    /// The order is the contract. The store is written *before* the state moves, so a
    /// daemon that dies in the middle comes back unpaired-in-memory and unpaired-on-disk
    /// rather than announcing an `Unpair()` a restart would undo — the mirror image of
    /// [`PairingStore::save_paired`] running before `Done`.
    ///
    /// **A backend that cannot stop listening does not fail this.** `Unpair()` destroys
    /// state the user asked to be rid of; refusing it because a vendor daemon would not
    /// shut down leaves them with a scanner they cannot unpair *and* a listener still
    /// running. Such a failure comes back as `Ok(Some(error))` — the association is gone,
    /// and the caller has something to log — rather than as an `Err` that would undo
    /// nothing.
    ///
    /// # Errors
    ///
    /// [`UnpairError::NotPaired`] when there is nothing to undo, which is what makes a
    /// second `Unpair()` distinguishable from the first; [`UnpairError::Store`] when the
    /// pairing could not be forgotten, in which case the scanner stays paired.
    pub async fn unpair(&self) -> Result<Option<BackendError>, UnpairError> {
        let scanner = {
            let mut inner = self.lock();
            if !inner.paired {
                return Err(UnpairError::NotPaired(inner.scanner.id.clone()));
            }
            // A pairing cannot be in flight while `paired` is set — `pair()` refuses —
            // so this only ever takes the finished run's handle.
            inner.task = None;
            inner.scanner.clone()
        };

        let stopped = self.backend.stop_listening(&scanner.id).await;
        // Revokes the backend's own pairing state (a token, a vendor config entry)
        // before the store entry is dropped, so nothing in between recognises a device
        // this call is in the middle of forgetting.
        let forgotten = self.backend.forget(&scanner.id).await;

        self.store
            .forget(&scanner.id)
            .await
            .map_err(|source| UnpairError::Store {
                scanner: scanner.id.clone(),
                source,
            })?;

        {
            let mut inner = self.lock();
            inner.paired = false;
            inner.state = PairingState::None;
        }
        let _ = self.transitions.send(PairingState::None);

        // Reported, not raised: see the note above on why a listener that would not
        // stop is not a reason to keep the pairing. A backend that could not forget its
        // own state is the same kind of failure, so it takes priority when both fail.
        Ok(forgotten.err().or_else(|| stopped.err()))
    }

    /// Marks the scanner paired without running the sequence — the restore path ([`4.2`]).
    ///
    /// A pairing that happened in an earlier run of the daemon is already durable, so
    /// there is nothing to install and nothing to save; what the object needs is the two
    /// properties a client reads, `Paired=true` and `PairingState="done"`. Announced
    /// through the transition stream like any other move, so the one place that emits
    /// `PropertiesChanged` stays the only place.
    ///
    /// A no-op on an already-paired machine.
    ///
    /// [`4.2`]: https://github.com/jeanparpaillon/scanbus_service/issues/17
    pub fn restore_paired(&self) {
        {
            let mut inner = self.lock();
            if inner.paired {
                return;
            }
            if let Some(task) = inner.task.take() {
                task.abort();
            }
            inner.paired = true;
            inner.state = PairingState::Done;
        }
        let _ = self.transitions.send(PairingState::Done);
    }

    /// Restores a scanner as present but not paired, with a startup reconciliation error.
    ///
    /// This is the restore-path mirror of a runtime pairing failure: the daemon store
    /// still names the scanner, so the object has to exist, but the backend is missing
    /// its half of the association and the user needs a visible "pair again".
    pub fn restore_failed(&self, message: impl Into<String>) {
        {
            let mut inner = self.lock();
            if let Some(task) = inner.task.take() {
                task.abort();
            }
            inner.paired = false;
            inner.state = PairingState::Failed(message.into());
        }
        let _ = self.transitions.send(self.state());
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().expect("pairing machine lock poisoned")
    }
}

/// Aborts whatever step is still running rather than let it outlive the machine.
///
/// Without this, dropping a [`PairingMachine`] mid-pairing would leave the spawned
/// driver task running to completion on its own — reaching the backend and the store
/// with nothing left to receive the transitions.
impl Drop for PairingMachine {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock()
            && let Some(task) = inner.task.take()
        {
            task.abort();
        }
    }
}

/// The driver task spawned by [`PairingMachine::pair`]: one run of §5's sequence.
async fn run(
    scanner: ScannerInfo,
    backend: Arc<dyn ScannerBackend>,
    store: Arc<dyn PairingStore>,
    inner: Arc<Mutex<Inner>>,
    transitions: broadcast::Sender<PairingState>,
) {
    let (progress_tx, mut progress_rx) = mpsc::channel::<PairingProgress>(PROGRESS_BUFFER);

    // Relays PairingProgress -> PairingState as it arrives, rather than waiting for
    // ensure_installed to return: that is the entire reason PairingState reaches
    // "installing_backend" while the install is still running, not after.
    let relay_inner = Arc::clone(&inner);
    let relay_transitions = transitions.clone();
    let relay = tokio::spawn(async move {
        while let Some(progress) = progress_rx.recv().await {
            if let Some(state) = progress.pairing_state() {
                relay_inner
                    .lock()
                    .expect("pairing machine lock poisoned")
                    .state = state.clone();
                let _ = relay_transitions.send(state);
            }
        }
    });

    let installed = backend.ensure_installed(&scanner, progress_tx).await;
    // ensure_installed has returned and dropped its sender; let the relay drain
    // whatever it last sent (its own Failed, if any) before this function overtakes it.
    let _ = relay.await;

    if let Err(error) = installed {
        finish(
            &inner,
            &transitions,
            PairingState::Failed(error.to_string()),
        );
        return;
    }

    let listening = backend.start_listening(&scanner).await;
    let stream = match listening {
        Ok(stream) => stream,
        Err(error) => {
            finish(
                &inner,
                &transitions,
                PairingState::Failed(error.to_string()),
            );
            return;
        }
    };
    // The daemon (2.3) is what consumes button events; this machine's job ends at
    // `Done`. Dropping the stream here stops the listener the same way a client
    // disconnecting would — harmless for a scanner that just finished pairing.
    drop(stream);

    if let Err(error) = store.save_paired(&scanner).await {
        finish(&inner, &transitions, PairingState::Failed(error.0));
        return;
    }

    // The store write above happens-before this: nothing here can announce Done
    // without having durably recorded the pairing first.
    {
        let mut guard = inner.lock().expect("pairing machine lock poisoned");
        guard.state = PairingState::Done;
        guard.paired = true;
        guard.task = None;
    }
    let _ = transitions.send(PairingState::Done);
}

/// Lands the machine on a terminal state and forgets the (now finished) task handle.
fn finish(
    inner: &Arc<Mutex<Inner>>,
    transitions: &broadcast::Sender<PairingState>,
    state: PairingState,
) {
    {
        let mut guard = inner.lock().expect("pairing machine lock poisoned");
        guard.state = state.clone();
        guard.task = None;
    }
    let _ = transitions.send(state);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use futures_core::stream::BoxStream;
    use tokio::sync::Mutex as AsyncMutex;

    use super::*;
    use crate::backend::ScanTrigger;
    use crate::backend::mock::{MockBackend, sample_scanner};
    use crate::error::BackendError;
    use crate::model::{ProfileKind, ScannerId, Value};
    use std::collections::BTreeMap;

    /// A [`PairingStore`] that records every scanner it was asked to save, and every one
    /// it was asked to forget.
    #[derive(Default)]
    struct RecordingStore {
        saved: AsyncMutex<Vec<ScannerId>>,
        forgotten: AsyncMutex<Vec<ScannerId>>,
    }

    #[async_trait]
    impl PairingStore for RecordingStore {
        async fn save_paired(&self, scanner: &ScannerInfo) -> Result<(), PairingStoreError> {
            self.saved.lock().await.push(scanner.id.clone());
            Ok(())
        }

        async fn forget(&self, scanner_id: &ScannerId) -> Result<(), PairingStoreError> {
            self.forgotten.lock().await.push(scanner_id.clone());
            Ok(())
        }
    }

    impl RecordingStore {
        async fn saved(&self) -> Vec<ScannerId> {
            self.saved.lock().await.clone()
        }

        async fn forgotten(&self) -> Vec<ScannerId> {
            self.forgotten.lock().await.clone()
        }
    }

    /// A [`PairingStore`] that always fails, with a fixed message.
    struct FailingStore(&'static str);

    #[async_trait]
    impl PairingStore for FailingStore {
        async fn save_paired(&self, _scanner: &ScannerInfo) -> Result<(), PairingStoreError> {
            Err(PairingStoreError::new(self.0))
        }

        async fn forget(&self, _scanner_id: &ScannerId) -> Result<(), PairingStoreError> {
            Err(PairingStoreError::new(self.0))
        }
    }

    /// Wraps [`MockBackend`], hanging forever on its *first* `ensure_installed` call
    /// right after reporting `Installing` — exactly the point `cancel()` has to be
    /// able to interrupt — and behaving normally on every later call.
    struct HangOnceBackend {
        inner: MockBackend,
        hung_once: AtomicBool,
        entries: AtomicU32,
    }

    #[async_trait]
    impl ScannerBackend for HangOnceBackend {
        fn id(&self) -> &'static str {
            self.inner.id()
        }

        async fn discover(&self) -> Result<Vec<ScannerInfo>, BackendError> {
            self.inner.discover().await
        }

        async fn ensure_installed(
            &self,
            scanner: &ScannerInfo,
            progress: mpsc::Sender<PairingProgress>,
        ) -> Result<(), BackendError> {
            self.entries.fetch_add(1, Ordering::SeqCst);
            if !self.hung_once.swap(true, Ordering::SeqCst) {
                let _ = progress
                    .send(PairingProgress::Installing {
                        package: "brscan5".to_owned(),
                        percent: None,
                    })
                    .await;
                std::future::pending::<()>().await;
                unreachable!("the pending future above never resolves");
            }
            self.inner.ensure_installed(scanner, progress).await
        }

        async fn start_listening(
            &self,
            scanner: &ScannerInfo,
        ) -> Result<BoxStream<'static, ScanTrigger>, BackendError> {
            self.inner.start_listening(scanner).await
        }

        async fn stop_listening(&self, scanner_id: &ScannerId) -> Result<(), BackendError> {
            self.inner.stop_listening(scanner_id).await
        }

        async fn set_button_mapping(
            &self,
            scanner_id: &ScannerId,
            button_index: u32,
            profile: Option<ProfileKind>,
            options: &BTreeMap<String, Value>,
        ) -> Result<(), BackendError> {
            self.inner
                .set_button_mapping(scanner_id, button_index, profile, options)
                .await
        }

        async fn fetch_pages(
            &self,
            scanner_id: &ScannerId,
            job_id: &str,
        ) -> Result<BoxStream<'static, Result<crate::model::RawPage, BackendError>>, BackendError>
        {
            self.inner.fetch_pages(scanner_id, job_id).await
        }
    }

    /// Waits (bounded, so a bug here fails the test instead of hanging CI) until
    /// `machine` reports `state`.
    ///
    /// Subscribes *before* reading the current state, which is the order the channel's
    /// lack of a starting value forces and the one that cannot miss a transition.
    async fn wait_for(machine: &PairingMachine, state: &PairingState) {
        let mut rx = machine.subscribe();
        if machine.state() == *state {
            return;
        }

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match rx.recv().await {
                    Ok(seen) if seen == *state => return,
                    Ok(_) => {}
                    Err(error) => panic!("no more transitions: {error}"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {state:?}, saw {:?}", machine.state()));
    }

    #[tokio::test]
    async fn cancel_during_installing_backend_ends_at_none_and_a_retry_runs_fully() {
        let backend = Arc::new(HangOnceBackend {
            inner: MockBackend::with_scanners([sample_scanner()]),
            hung_once: AtomicBool::new(false),
            entries: AtomicU32::new(0),
        });
        let handle = backend.inner.handle();
        handle.set_install_packages(["brscan5"]);
        let store = Arc::new(RecordingStore::default());
        let backend_ref = Arc::clone(&backend);
        let machine = PairingMachine::new(sample_scanner(), backend, store.clone());

        assert_eq!(machine.pair(), PairOutcome::Started);
        wait_for(&machine, &PairingState::InstallingBackend).await;
        assert_eq!(backend_ref.entries.load(Ordering::SeqCst), 1);

        machine.cancel();
        assert_eq!(machine.state(), PairingState::None);
        assert!(!machine.is_paired());

        // A retry does not see the aborted attempt: it runs ensure_installed again,
        // this time to completion, and reaches Done.
        assert_eq!(machine.pair(), PairOutcome::Started);
        wait_for(&machine, &PairingState::Done).await;
        assert!(machine.is_paired());
        assert_eq!(backend_ref.entries.load(Ordering::SeqCst), 2);
        assert_eq!(handle.install_attempts(), 1);
        assert_eq!(store.saved().await, vec![sample_scanner().id]);
    }

    #[tokio::test]
    async fn two_concurrent_pairs_run_ensure_installed_exactly_once() {
        let backend = MockBackend::with_scanners([sample_scanner()]);
        let handle = backend.handle();
        handle.set_install_packages(["brscan5"]);
        let store = Arc::new(RecordingStore::default());
        let machine = PairingMachine::new(sample_scanner(), Arc::new(backend), store);

        assert_eq!(machine.pair(), PairOutcome::Started);
        assert_eq!(machine.pair(), PairOutcome::AlreadyInProgress);

        wait_for(&machine, &PairingState::Done).await;
        assert_eq!(handle.install_attempts(), 1);

        // Pair()-ing an already-paired scanner is its own outcome, not "in progress".
        assert_eq!(machine.pair(), PairOutcome::AlreadyPaired);
        assert_eq!(handle.install_attempts(), 1);
    }

    #[tokio::test]
    async fn ensure_installed_failing_lands_on_failed_and_never_calls_start_listening() {
        let backend = MockBackend::with_scanners([sample_scanner()]);
        let handle = backend.handle();
        let error = BackendError::InstallFailed {
            package: "brscan5".to_owned(),
            detail: "404".to_owned(),
        };
        handle.fail_ensure_installed(error.clone());
        let store = Arc::new(RecordingStore::default());
        let machine = PairingMachine::new(sample_scanner(), Arc::new(backend), store.clone());

        assert_eq!(machine.pair(), PairOutcome::Started);
        wait_for(&machine, &PairingState::Failed(error.to_string())).await;

        assert!(!machine.is_paired());
        assert!(
            !handle
                .calls()
                .iter()
                .any(|call| matches!(call, crate::backend::mock::MockCall::StartListening(_)))
        );
        assert!(store.saved().await.is_empty());

        // Failed stays retryable: a fresh pair() runs the sequence again.
        handle.succeed_ensure_installed();
        assert_eq!(machine.pair(), PairOutcome::Started);
        wait_for(&machine, &PairingState::Done).await;
        assert!(machine.is_paired());
    }

    #[tokio::test]
    async fn the_store_is_written_before_done_is_emitted() {
        let backend = MockBackend::with_scanners([sample_scanner()]);
        let store = Arc::new(RecordingStore::default());
        let machine = PairingMachine::new(sample_scanner(), Arc::new(backend), store.clone());

        let mut rx = machine.subscribe();
        assert_eq!(machine.pair(), PairOutcome::Started);

        loop {
            let state = rx.recv().await.expect("machine dropped its sender");
            if state == PairingState::Done {
                // By the time Done is observable, the write has already happened —
                // there is no `.await` between the store write and this send.
                assert_eq!(store.saved().await, vec![sample_scanner().id]);
                break;
            }
        }
    }

    #[tokio::test]
    async fn a_store_failure_lands_on_failed_instead_of_done() {
        let backend = MockBackend::with_scanners([sample_scanner()]);
        let machine = PairingMachine::new(
            sample_scanner(),
            Arc::new(backend),
            Arc::new(FailingStore("disk full")),
        );

        assert_eq!(machine.pair(), PairOutcome::Started);
        wait_for(&machine, &PairingState::Failed("disk full".to_owned())).await;
        assert!(!machine.is_paired());
    }

    /// `Unpair()` from the machine's side: the listener stops, the store forgets, and the
    /// two properties a client watches go back to where a fresh scanner starts.
    #[tokio::test]
    async fn unpairing_stops_the_listener_forgets_the_store_and_lands_on_none() {
        let backend = MockBackend::with_scanners([sample_scanner()]);
        let handle = backend.handle();
        let store = Arc::new(RecordingStore::default());
        let machine = PairingMachine::new(sample_scanner(), Arc::new(backend), store.clone());

        assert_eq!(machine.pair(), PairOutcome::Started);
        wait_for(&machine, &PairingState::Done).await;
        handle.clear_calls();

        assert_eq!(machine.unpair().await, Ok(None));
        assert!(!machine.is_paired());
        assert_eq!(machine.state(), PairingState::None);
        assert_eq!(store.forgotten().await, vec![sample_scanner().id]);
        assert_eq!(
            handle.calls(),
            vec![
                crate::backend::mock::MockCall::StopListening {
                    scanner: sample_scanner().id,
                    // The machine dropped the stream at `Done` (see `run`), so there is
                    // nothing live to stop until 2.4 owns the listener task.
                    was_listening: false,
                },
                crate::backend::mock::MockCall::Forget(sample_scanner().id),
            ]
        );

        // And it is pairable again, from scratch.
        assert_eq!(machine.pair(), PairOutcome::Started);
        wait_for(&machine, &PairingState::Done).await;
        assert!(machine.is_paired());
    }

    /// The second `Unpair()` has nothing to undo and says so — the distinction the daemon
    /// turns into `org.scanbus.Error.NotPaired`.
    #[tokio::test]
    async fn unpairing_something_that_is_not_paired_is_not_paired() {
        let backend = MockBackend::with_scanners([sample_scanner()]);
        let store = Arc::new(RecordingStore::default());
        let machine = PairingMachine::new(sample_scanner(), Arc::new(backend), store.clone());

        assert_eq!(
            machine.unpair().await,
            Err(UnpairError::NotPaired(sample_scanner().id))
        );
        assert!(store.forgotten().await.is_empty());

        machine.pair();
        wait_for(&machine, &PairingState::Done).await;
        machine.unpair().await.expect("the first unpair succeeds");
        assert_eq!(
            machine.unpair().await,
            Err(UnpairError::NotPaired(sample_scanner().id))
        );
    }

    /// A pairing the store would not forget must not be announced as gone: a restart
    /// would bring it back.
    #[tokio::test]
    async fn a_store_that_cannot_forget_leaves_the_scanner_paired() {
        struct SaveButNeverForget;

        #[async_trait]
        impl PairingStore for SaveButNeverForget {
            async fn save_paired(&self, _scanner: &ScannerInfo) -> Result<(), PairingStoreError> {
                Ok(())
            }

            async fn forget(&self, _scanner_id: &ScannerId) -> Result<(), PairingStoreError> {
                Err(PairingStoreError::new("read-only file system"))
            }
        }

        let backend = MockBackend::with_scanners([sample_scanner()]);
        let machine = PairingMachine::new(
            sample_scanner(),
            Arc::new(backend),
            Arc::new(SaveButNeverForget),
        );

        machine.pair();
        wait_for(&machine, &PairingState::Done).await;

        assert_eq!(
            machine.unpair().await,
            Err(UnpairError::Store {
                scanner: sample_scanner().id,
                source: PairingStoreError::new("read-only file system"),
            })
        );
        assert!(machine.is_paired());
        assert_eq!(machine.state(), PairingState::Done);
    }

    /// A listener that refuses to stop is reported, not raised: the pairing still goes.
    #[tokio::test]
    async fn a_listener_that_will_not_stop_does_not_block_the_unpairing() {
        struct StubbornListener(MockBackend);

        #[async_trait]
        impl ScannerBackend for StubbornListener {
            fn id(&self) -> &'static str {
                self.0.id()
            }

            async fn discover(&self) -> Result<Vec<ScannerInfo>, BackendError> {
                self.0.discover().await
            }

            async fn ensure_installed(
                &self,
                scanner: &ScannerInfo,
                progress: mpsc::Sender<PairingProgress>,
            ) -> Result<(), BackendError> {
                self.0.ensure_installed(scanner, progress).await
            }

            async fn start_listening(
                &self,
                scanner: &ScannerInfo,
            ) -> Result<BoxStream<'static, ScanTrigger>, BackendError> {
                self.0.start_listening(scanner).await
            }

            async fn stop_listening(&self, _scanner_id: &ScannerId) -> Result<(), BackendError> {
                Err(BackendError::Other("brscan-skey will not die".to_owned()))
            }

            async fn set_button_mapping(
                &self,
                scanner_id: &ScannerId,
                button_index: u32,
                profile: Option<ProfileKind>,
                options: &BTreeMap<String, Value>,
            ) -> Result<(), BackendError> {
                self.0
                    .set_button_mapping(scanner_id, button_index, profile, options)
                    .await
            }

            async fn fetch_pages(
                &self,
                scanner_id: &ScannerId,
                job_id: &str,
            ) -> Result<BoxStream<'static, Result<crate::model::RawPage, BackendError>>, BackendError>
            {
                self.0.fetch_pages(scanner_id, job_id).await
            }
        }

        let backend = StubbornListener(MockBackend::with_scanners([sample_scanner()]));
        let store = Arc::new(RecordingStore::default());
        let machine = PairingMachine::new(sample_scanner(), Arc::new(backend), store.clone());

        machine.pair();
        wait_for(&machine, &PairingState::Done).await;

        assert_eq!(
            machine.unpair().await,
            Ok(Some(BackendError::Other(
                "brscan-skey will not die".to_owned()
            )))
        );
        assert!(!machine.is_paired());
        assert_eq!(store.forgotten().await, vec![sample_scanner().id]);
    }

    /// A backend that cannot revoke its own pairing state is reported the same way as a
    /// listener that will not stop: the pairing still goes.
    #[tokio::test]
    async fn a_backend_that_cannot_forget_does_not_block_the_unpairing() {
        struct RefusesToForget(MockBackend);

        #[async_trait]
        impl ScannerBackend for RefusesToForget {
            fn id(&self) -> &'static str {
                self.0.id()
            }

            async fn discover(&self) -> Result<Vec<ScannerInfo>, BackendError> {
                self.0.discover().await
            }

            async fn ensure_installed(
                &self,
                scanner: &ScannerInfo,
                progress: mpsc::Sender<PairingProgress>,
            ) -> Result<(), BackendError> {
                self.0.ensure_installed(scanner, progress).await
            }

            async fn start_listening(
                &self,
                scanner: &ScannerInfo,
            ) -> Result<BoxStream<'static, ScanTrigger>, BackendError> {
                self.0.start_listening(scanner).await
            }

            async fn stop_listening(&self, scanner_id: &ScannerId) -> Result<(), BackendError> {
                self.0.stop_listening(scanner_id).await
            }

            async fn forget(&self, _scanner_id: &ScannerId) -> Result<(), BackendError> {
                Err(BackendError::Other("device store is read-only".to_owned()))
            }

            async fn set_button_mapping(
                &self,
                scanner_id: &ScannerId,
                button_index: u32,
                profile: Option<ProfileKind>,
                options: &BTreeMap<String, Value>,
            ) -> Result<(), BackendError> {
                self.0
                    .set_button_mapping(scanner_id, button_index, profile, options)
                    .await
            }

            async fn fetch_pages(
                &self,
                scanner_id: &ScannerId,
                job_id: &str,
            ) -> Result<BoxStream<'static, Result<crate::model::RawPage, BackendError>>, BackendError>
            {
                self.0.fetch_pages(scanner_id, job_id).await
            }
        }

        let backend = RefusesToForget(MockBackend::with_scanners([sample_scanner()]));
        let store = Arc::new(RecordingStore::default());
        let machine = PairingMachine::new(sample_scanner(), Arc::new(backend), store.clone());

        machine.pair();
        wait_for(&machine, &PairingState::Done).await;

        assert_eq!(
            machine.unpair().await,
            Ok(Some(BackendError::Other(
                "device store is read-only".to_owned()
            )))
        );
        assert!(!machine.is_paired());
        assert_eq!(store.forgotten().await, vec![sample_scanner().id]);
    }

    /// The restore path (4.2): paired without a sequence, announced like any transition.
    #[tokio::test]
    async fn restoring_a_pairing_announces_done_without_touching_the_backend() {
        let backend = MockBackend::with_scanners([sample_scanner()]);
        let handle = backend.handle();
        let store = Arc::new(RecordingStore::default());
        let machine = PairingMachine::new(sample_scanner(), Arc::new(backend), store.clone());
        let mut transitions = machine.subscribe();

        machine.restore_paired();

        assert!(machine.is_paired());
        assert_eq!(machine.state(), PairingState::Done);
        assert_eq!(transitions.try_recv().unwrap(), PairingState::Done);
        assert!(handle.calls().is_empty(), "{:?}", handle.calls());
        assert!(store.saved().await.is_empty());

        // Idempotent, and `Pair()` on it is §9's AlreadyPaired.
        machine.restore_paired();
        assert_eq!(machine.pair(), PairOutcome::AlreadyPaired);
    }

    #[tokio::test]
    async fn restoring_a_failed_pairing_leaves_the_object_unpaired_with_a_message() {
        let backend = MockBackend::with_scanners([sample_scanner()]);
        let handle = backend.handle();
        let store = Arc::new(RecordingStore::default());
        let machine = PairingMachine::new(sample_scanner(), Arc::new(backend), store.clone());
        let mut transitions = machine.subscribe();

        machine.restore_failed("the pairing secret is missing; pair the phone again");

        assert!(!machine.is_paired());
        assert_eq!(
            machine.state(),
            PairingState::Failed("the pairing secret is missing; pair the phone again".to_owned())
        );
        assert_eq!(
            transitions.try_recv().unwrap(),
            PairingState::Failed("the pairing secret is missing; pair the phone again".to_owned())
        );
        assert!(handle.calls().is_empty(), "{:?}", handle.calls());
        assert!(store.saved().await.is_empty());
    }

    /// A rediscovery moves the address the next `pair()` will use, and nothing else.
    #[tokio::test]
    async fn set_scanner_updates_what_a_later_pair_is_run_against() {
        let backend = MockBackend::with_scanners([sample_scanner()]);
        let store = Arc::new(RecordingStore::default());
        let machine = PairingMachine::new(sample_scanner(), Arc::new(backend), store.clone());

        let mut moved = sample_scanner();
        moved.address = "usb:001:003".to_owned();
        let previous = machine.set_scanner(moved.clone());

        assert_eq!(previous.address, "usb:001:002");
        assert_eq!(machine.scanner(), moved);
        assert_eq!(machine.state(), PairingState::None);
    }

    #[tokio::test]
    async fn dropping_the_machine_mid_pairing_does_not_panic_or_leak_the_task() {
        let attempts = Arc::new(AtomicU32::new(0));

        struct HangingForever {
            attempts: Arc<AtomicU32>,
        }

        #[async_trait]
        impl ScannerBackend for HangingForever {
            fn id(&self) -> &'static str {
                "hanging"
            }

            async fn discover(&self) -> Result<Vec<ScannerInfo>, BackendError> {
                Ok(Vec::new())
            }

            async fn ensure_installed(
                &self,
                _scanner: &ScannerInfo,
                _progress: mpsc::Sender<PairingProgress>,
            ) -> Result<(), BackendError> {
                self.attempts.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
                unreachable!("the pending future above never resolves");
            }

            async fn start_listening(
                &self,
                _scanner: &ScannerInfo,
            ) -> Result<BoxStream<'static, ScanTrigger>, BackendError> {
                unreachable!("cancelled before ensure_installed returns")
            }

            async fn stop_listening(&self, _scanner_id: &ScannerId) -> Result<(), BackendError> {
                Ok(())
            }

            async fn set_button_mapping(
                &self,
                _scanner_id: &ScannerId,
                _button_index: u32,
                _profile: Option<ProfileKind>,
                _options: &BTreeMap<String, Value>,
            ) -> Result<(), BackendError> {
                Ok(())
            }

            async fn fetch_pages(
                &self,
                _scanner_id: &ScannerId,
                _job_id: &str,
            ) -> Result<BoxStream<'static, Result<crate::model::RawPage, BackendError>>, BackendError>
            {
                unreachable!("not exercised by this test")
            }
        }

        let backend = Arc::new(HangingForever {
            attempts: Arc::clone(&attempts),
        });
        let store = Arc::new(RecordingStore::default());
        let machine = PairingMachine::new(sample_scanner(), backend, store);

        assert_eq!(machine.pair(), PairOutcome::Started);
        // Unlike `wait_for`, `PairingState::Pairing` is set synchronously by `pair()`
        // itself, before the driver task has necessarily been polled even once — so
        // wait for the concrete side effect the task performs instead.
        tokio::time::timeout(Duration::from_secs(5), async {
            while attempts.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("ensure_installed was never entered");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);

        drop(machine);

        // Give the aborted task a chance to actually be torn down by the runtime; if
        // it were leaked, `attempts` would keep the process from ever finishing, but
        // there is nothing left observable to increment — this just asserts the drop
        // itself did not panic.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
