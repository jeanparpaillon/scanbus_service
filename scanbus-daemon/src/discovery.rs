//! [`Discovery`]: the session behind `StartDiscovery`/`StopDiscovery`.
//!
//! §2 gives `StartDiscovery` no return value, and §1 gives the scanners it finds a
//! lifetime bounded by the session. So the call is not "probe and answer" — it starts
//! something that runs, publishes objects as it goes, and is taken down by
//! `StopDiscovery`. Everything a client learns arrives as `InterfacesAdded`.
//!
//! # Why the call returns before the first probe
//!
//! A probe takes seconds: an mDNS browse waits out its response window, a SANE
//! enumeration walks USB. The default D-Bus reply timeout is 25 s, and a handler that
//! awaited even one round would make `StartDiscovery` fail on a machine with a slow
//! backend — the same mistake §9 forbids for `Pair()`. The session is therefore a task,
//! and the method returns as soon as it is spawned.
//!
//! # Rounds, and ending on its own
//!
//! One probe is not enough: a scanner powered on five seconds after the call would
//! never appear, and a client's discovery UI would be wrong for as long as it stays
//! open. The session re-probes every [`PROBE_INTERVAL`] until it is stopped or
//! [`SESSION_LIMIT`] elapses. The limit is a stopgap: until [`2.9`] ties the session to
//! the bus names that asked for it, a client that is killed without calling
//! `StopDiscovery` would otherwise leave the daemon probing the network forever.
//!
//! # One broken backend is not a broken discovery
//!
//! Backends are probed concurrently, each on its own task with its own
//! [`BACKEND_TIMEOUT`]. A backend that errors, times out or panics is logged and
//! skipped, and the scanners the others found still appear: a missing `scanimage` is an
//! environment fact, not a reason for the call to fail. Contrast the `filters` argument,
//! where a name that matches no backend *is* a client bug and is refused
//! ([`crate::backends`]).
//!
//! [`2.9`]: https://github.com/jeanparpaillon/scanbus_service/issues/34

use std::sync::Arc;
use std::time::{Duration, Instant};

use scanbus_core::ScannerBackend;
use tokio::sync::{Mutex, watch};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, info, instrument, warn};

use crate::backends::{Backends, RankedBackend, UnknownBackend};
use crate::scanners::ScannerRegistry;

/// How long one backend's `discover()` may take before the round gives up on it.
///
/// Per backend and per round, so a hung SANE never delays the scanners another backend
/// already found — the round is only as slow as its slowest *answering* backend.
pub const BACKEND_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a session waits between probe rounds.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// How long a session runs before ending on its own.
pub const SESSION_LIMIT: Duration = Duration::from_secs(300);

/// The discovery session: at most one at a time, restarted by nothing.
pub struct Discovery {
    backends: Backends,
    scanners: Arc<ScannerRegistry>,
    session: Mutex<Option<Session>>,
}

/// A running session, from the outside.
struct Session {
    /// Set to `true` to ask the task to stop; also tells a round that finished probing
    /// not to publish what it found.
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Discovery {
    /// A discovery that probes `backends` and publishes into `scanners`.
    pub fn new(backends: Backends, scanners: Arc<ScannerRegistry>) -> Self {
        Self {
            backends,
            scanners,
            session: Mutex::new(None),
        }
    }

    /// The backends this daemon can probe, in precedence order.
    pub fn backends(&self) -> &Backends {
        &self.backends
    }

    /// Starts a session, or leaves the running one alone.
    ///
    /// §2's "a second `StartDiscovery` restarts nothing and returns successfully": the
    /// filters of the second call are *not* applied to the running session, because
    /// restarting it would remove and re-add every unpaired object the first client is
    /// watching. [`2.9`](self) is what makes that sharing explicit.
    ///
    /// # Errors
    ///
    /// [`UnknownBackend`] if `backend_filter` names something this daemon does not
    /// have. Checked before anything is started, so a refused call changes nothing.
    #[instrument(level = "info", skip_all)]
    pub async fn start(&self, backend_filter: Option<&[String]>) -> Result<(), UnknownBackend> {
        let selected = self.backends.select(backend_filter)?;
        let mut session = self.session.lock().await;

        if session.as_ref().is_some_and(|s| !s.task.is_finished()) {
            info!("discovery is already running; joining it");
            return Ok(());
        }

        if selected.is_empty() {
            warn!(
                "no backend to probe; discovery will find nothing \
                 (a default build compiles none in)"
            );
        }

        let (stop, stopped) = watch::channel(false);
        let scanners = Arc::clone(&self.scanners);
        let backends: Vec<&'static str> = selected.iter().map(|e| e.backend.id()).collect();

        info!(?backends, "discovery started");
        let task = tokio::spawn(run_session(selected, scanners, stopped));
        *session = Some(Session { stop, task });

        Ok(())
    }

    /// Stops the session and removes the objects that existed only for it.
    ///
    /// Both halves are awaited before this returns, so a client that called
    /// `StopDiscovery` and then `GetManagedObjects` cannot see an unpaired scanner that
    /// is on its way out. Succeeds when nothing is running: stopping a session that
    /// already ended on its own is not an error, and neither is a client's redundant
    /// `StopDiscovery`.
    #[instrument(level = "info", skip_all)]
    pub async fn stop(&self) {
        let session = self.session.lock().await.take();

        if let Some(session) = session {
            // Ignored: a receiver-less channel means the task has already exited.
            let _ = session.stop.send(true);
            if let Err(error) = session.task.await {
                warn!(%error, "the discovery task did not end cleanly");
            }
        }

        // Unconditionally, even with no session: a task that ended on its own has
        // already done this, and doing it twice removes nothing the second time.
        self.scanners.end_discovery().await;
        info!("discovery stopped");
    }

    /// Whether a session is running right now.
    pub async fn is_running(&self) -> bool {
        self.session
            .lock()
            .await
            .as_ref()
            .is_some_and(|session| !session.task.is_finished())
    }
}

/// The session task: probe, publish, wait, repeat, until stopped or out of time.
async fn run_session(
    backends: Vec<RankedBackend>,
    scanners: Arc<ScannerRegistry>,
    mut stopped: watch::Receiver<bool>,
) {
    let deadline = Instant::now() + SESSION_LIMIT;

    loop {
        let found = probe(&backends).await;

        // Checked after the probe rather than before publishing each scanner: a
        // `StopDiscovery` that arrived while the round was in flight must not be
        // followed by an `InterfacesAdded` for a scanner it was supposed to take away.
        if *stopped.borrow() {
            break;
        }

        for (rank, info) in found {
            if let Err(error) = scanners.observe(rank, info).await {
                warn!(%error, "could not publish a discovered scanner");
            }
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            info!(
                limit_secs = SESSION_LIMIT.as_secs(),
                "discovery session reached its limit; ending"
            );
            // The session ends the way `StopDiscovery` ends it: §1 bounds these objects
            // by the session, not by how the session finished.
            scanners.end_discovery().await;
            break;
        }

        tokio::select! {
            _ = tokio::time::sleep(remaining.min(PROBE_INTERVAL)) => {}
            _ = stopped.changed() => break,
        }
    }
}

/// One round: every backend at once, each with its own timeout.
///
/// The results are ordered by rank so that the better-ranked sighting of a device is
/// the one [`ScannerRegistry::observe`](crate::scanners::ScannerRegistry::observe)
/// sees first, which is what makes the precedence order decide a tie.
async fn probe(backends: &[RankedBackend]) -> Vec<(usize, scanbus_core::ScannerInfo)> {
    let mut tasks = JoinSet::new();

    for entry in backends {
        let rank = entry.rank;
        let backend: Arc<dyn ScannerBackend> = Arc::clone(&entry.backend);

        tasks.spawn(async move {
            let id = backend.id();
            let outcome = tokio::time::timeout(BACKEND_TIMEOUT, backend.discover()).await;

            match outcome {
                Ok(Ok(scanners)) => {
                    debug!(backend = id, found = scanners.len(), "backend probed");
                    scanners.into_iter().map(|info| (rank, info)).collect()
                }
                Ok(Err(error)) => {
                    // Logged and skipped: a backend that cannot probe is an environment
                    // fact, and the other backends' scanners still have to appear.
                    warn!(backend = id, %error, "backend discovery failed; skipping it");
                    Vec::new()
                }
                Err(_) => {
                    warn!(
                        backend = id,
                        timeout_secs = BACKEND_TIMEOUT.as_secs(),
                        "backend discovery timed out; skipping it"
                    );
                    Vec::new()
                }
            }
        });
    }

    let mut found = Vec::new();
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(scanners) => found.extend(scanners),
            // A panicking backend takes down its own probe and nothing else.
            Err(error) => warn!(%error, "a backend probe panicked"),
        }
    }

    found.sort_by_key(|(rank, _)| *rank);
    found
}
