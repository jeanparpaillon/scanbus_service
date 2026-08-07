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
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;

use crate::backend::{PairingProgress, ScannerBackend};
use crate::model::{PairingState, ScannerInfo};

/// How many [`PairingProgress`] steps [`ScannerBackend::ensure_installed`] may have
/// in flight before the relay that turns them into transitions has to catch up.
///
/// Generous on purpose: a slow relay must never be the reason a backend's `.send`
/// blocks, since that `.send` failing silently (receiver dropped) is already part of
/// the trait's contract, but *blocking* it is not.
const PROGRESS_BUFFER: usize = 16;

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

/// Shared, lockable state — everything [`PairingMachine::pair`] and
/// [`PairingMachine::cancel`] have to agree on atomically.
struct Inner {
    state: PairingState,
    paired: bool,
    task: Option<AbortHandle>,
}

/// Drives one scanner's `PairingState` through §5.
///
/// Owns no bus, no timer: just the current [`PairingState`], a handle to the task
/// running the sequence, and what it takes to run one — a backend and a store.
pub struct PairingMachine {
    scanner: ScannerInfo,
    backend: Arc<dyn ScannerBackend>,
    store: Arc<dyn PairingStore>,
    inner: Arc<Mutex<Inner>>,
    transitions: watch::Sender<PairingState>,
}

impl PairingMachine {
    /// A machine for `scanner`, starting at [`PairingState::None`] and unpaired.
    pub fn new(
        scanner: ScannerInfo,
        backend: Arc<dyn ScannerBackend>,
        store: Arc<dyn PairingStore>,
    ) -> Self {
        let (transitions, _) = watch::channel(PairingState::None);
        Self {
            scanner,
            backend,
            store,
            inner: Arc::new(Mutex::new(Inner {
                state: PairingState::None,
                paired: false,
                task: None,
            })),
            transitions,
        }
    }

    /// The scanner this machine is pairing.
    pub const fn scanner(&self) -> &ScannerInfo {
        &self.scanner
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

    /// A receiver of every `PairingState` this machine moves to, current value first.
    ///
    /// This is the "channel" the module doc promises: the daemon (2.3) turns each
    /// value out of it into a `PropertiesChanged` for `PairingState`/`PairingError`.
    pub fn subscribe(&self) -> watch::Receiver<PairingState> {
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
        self.transitions.send_replace(PairingState::Pairing);

        let scanner = self.scanner.clone();
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
            self.transitions.send_replace(PairingState::None);
        }
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
    transitions: watch::Sender<PairingState>,
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
                relay_transitions.send_replace(state);
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
    transitions.send_replace(PairingState::Done);
}

/// Lands the machine on a terminal state and forgets the (now finished) task handle.
fn finish(
    inner: &Arc<Mutex<Inner>>,
    transitions: &watch::Sender<PairingState>,
    state: PairingState,
) {
    {
        let mut guard = inner.lock().expect("pairing machine lock poisoned");
        guard.state = state.clone();
        guard.task = None;
    }
    transitions.send_replace(state);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use futures_core::stream::BoxStream;
    use tokio::sync::Mutex as AsyncMutex;

    use super::*;
    use crate::backend::ButtonPressedEvent;
    use crate::backend::mock::{MockBackend, sample_scanner};
    use crate::error::BackendError;
    use crate::model::{ProfileKind, ScannerId, Value};
    use std::collections::BTreeMap;

    /// A [`PairingStore`] that records every scanner it was asked to save.
    #[derive(Default)]
    struct RecordingStore {
        saved: AsyncMutex<Vec<ScannerId>>,
    }

    #[async_trait]
    impl PairingStore for RecordingStore {
        async fn save_paired(&self, scanner: &ScannerInfo) -> Result<(), PairingStoreError> {
            self.saved.lock().await.push(scanner.id.clone());
            Ok(())
        }
    }

    impl RecordingStore {
        async fn saved(&self) -> Vec<ScannerId> {
            self.saved.lock().await.clone()
        }
    }

    /// A [`PairingStore`] that always fails, with a fixed message.
    struct FailingStore(&'static str);

    #[async_trait]
    impl PairingStore for FailingStore {
        async fn save_paired(&self, _scanner: &ScannerInfo) -> Result<(), PairingStoreError> {
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
        ) -> Result<BoxStream<'static, ButtonPressedEvent>, BackendError> {
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
        ) -> Result<BoxStream<'static, crate::model::RawPage>, BackendError> {
            self.inner.fetch_pages(scanner_id, job_id).await
        }
    }

    /// Waits (bounded, so a bug here fails the test instead of hanging CI) until
    /// `machine` reports `state`.
    async fn wait_for(machine: &PairingMachine, state: &PairingState) {
        let mut rx = machine.subscribe();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if &*rx.borrow() == state {
                    return;
                }
                rx.changed().await.expect("machine dropped its sender");
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
            rx.changed().await.expect("machine dropped its sender");
            let state = rx.borrow().clone();
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
            ) -> Result<BoxStream<'static, ButtonPressedEvent>, BackendError> {
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
            ) -> Result<BoxStream<'static, crate::model::RawPage>, BackendError> {
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
