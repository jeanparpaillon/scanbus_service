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
//! [`SESSION_LIMIT`] elapses. The limit is now only a backstop — [`2.9`] ties the
//! session to the bus names that asked for it (below), so a killed client's leak is
//! closed well before it — but a session held open by a client that is merely wedged,
//! never crashing, still has to end somewhere.
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
//! # Whose session is it
//!
//! [`2.9`] reference-counts the session by the caller's unique bus name: `start` adds
//! the sender, `release` drops it, and the probe only actually stops when the set is
//! empty. A second `StartDiscovery` therefore joins the running session rather than
//! restarting it, from *either* end — §2 already required that of the running session,
//! this is what makes it true of `StopDiscovery` too, so one client's `Ctrl-C` cannot
//! take away the objects a second client is watching. [`watch_owners`] is the other
//! half: a client that never calls `StopDiscovery` because it crashed or was killed
//! still owns nothing once `NameOwnerChanged` says its name is gone.
//!
//! [`2.9`]: https://github.com/jeanparpaillon/scanbus_service/issues/34

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use scanbus_core::ScannerBackend;
use tokio::sync::{Mutex, watch};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, info, instrument, warn};
use zbus::Connection;
use zbus::names::{BusName, OwnedUniqueName, UniqueName};

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
    state: Mutex<State>,
}

/// A running session, from the outside.
struct Session {
    /// Set to `true` to ask the task to stop; also tells a round that finished probing
    /// not to publish what it found.
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

/// The session, and who is holding it open.
#[derive(Default)]
struct State {
    session: Option<Session>,
    /// The unique bus names that called `StartDiscovery` and have not since called
    /// `StopDiscovery` (or disappeared — see [`Discovery::owner_gone`]). The probe runs
    /// while this is non-empty and stops the moment it is not.
    owners: HashSet<OwnedUniqueName>,
}

impl Discovery {
    /// A discovery that probes `backends` and publishes into `scanners`.
    pub fn new(backends: Backends, scanners: Arc<ScannerRegistry>) -> Self {
        Self {
            backends,
            scanners,
            state: Mutex::new(State::default()),
        }
    }

    /// The backends this daemon can probe, in precedence order.
    pub fn backends(&self) -> &Backends {
        &self.backends
    }

    /// Adds `owner`'s reference and starts a session, or leaves the running one alone.
    ///
    /// §2's "a second `StartDiscovery` restarts nothing and returns successfully": the
    /// filters of the second call are *not* applied to the running session, because
    /// restarting it would remove and re-add every unpaired object the first client is
    /// watching. `owner` still joins the set that keeps the session open, so a later
    /// `StopDiscovery` from the *first* client leaves it running for the second
    /// ([`2.9`](self)).
    ///
    /// # Errors
    ///
    /// [`UnknownBackend`] if `backend_filter` names something this daemon does not
    /// have. Checked before anything is started, so a refused call changes nothing —
    /// not even `owner`'s reference.
    #[instrument(level = "info", skip_all)]
    pub async fn start(
        &self,
        owner: OwnedUniqueName,
        backend_filter: Option<&[String]>,
    ) -> Result<(), UnknownBackend> {
        let selected = self.backends.select(backend_filter)?;
        let mut state = self.state.lock().await;

        if state.owners.insert(owner.clone()) {
            info!(%owner, owners = state.owners.len(), "discovery reference taken");
        } else {
            debug!(%owner, "StartDiscovery from a client that already holds a reference");
        }

        if state
            .session
            .as_ref()
            .is_some_and(|s| !s.task.is_finished())
        {
            info!(
                owners = state.owners.len(),
                "discovery is already running; joining it"
            );
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
        state.session = Some(Session { stop, task });

        Ok(())
    }

    /// Drops `owner`'s reference, stopping the session and removing the objects that
    /// existed only for it once no reference is left.
    ///
    /// Not an error to call for an `owner` that never called `start` — that owns
    /// nothing, so releasing it changes nothing, which is what a redundant
    /// `StopDiscovery` needs to be a no-op rather than a bug report. When this *is* the
    /// last reference, both halves of the stop are awaited before returning, so a
    /// client that called `StopDiscovery` and then `GetManagedObjects` cannot see an
    /// unpaired scanner that is on its way out.
    #[instrument(level = "info", skip_all)]
    pub async fn release(&self, owner: &UniqueName<'_>) {
        self.release_owner(owner, "StopDiscovery").await;
    }

    /// Stops the session unconditionally, dropping every reference to it.
    ///
    /// For daemon shutdown, not for a client's `StopDiscovery` — see [`release`](Self::release)
    /// for that. Succeeds when nothing is running: stopping a session that already ended
    /// on its own is not an error.
    #[instrument(level = "info", skip_all)]
    pub async fn stop(&self) {
        let mut state = self.state.lock().await;
        state.owners.clear();
        let session = state.session.take();

        self.stop_session(session).await;
    }

    /// One reference off, with `reason` naming what took it away — the log line that
    /// answers "why is discovery still running", or "why did it stop".
    async fn release_owner(&self, owner: &UniqueName<'_>, reason: &'static str) {
        // The lock is held across the stop rather than dropped first: a `start` landing
        // in between would find `session` already taken, spawn a second one, and then
        // watch the `end_discovery` still on its way out sweep the objects it published.
        let mut state = self.state.lock().await;

        if !state.owners.remove(owner) {
            debug!(%owner, reason, "release from a client with no discovery reference; ignoring");
            return;
        }

        info!(%owner, reason, owners_left = state.owners.len(), "discovery reference dropped");
        if !state.owners.is_empty() {
            return;
        }

        let session = state.session.take();
        self.stop_session(session).await;
    }

    /// The shared tail of `release` and `stop`: signal the task, wait for it, then sweep
    /// the objects the session leaves behind.
    async fn stop_session(&self, session: Option<Session>) {
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

    /// Drops `owner`'s reference because `NameOwnerChanged` said it is gone, not because
    /// it called `StopDiscovery`.
    ///
    /// Same bookkeeping as [`release`](Self::release) — a crashed client owns exactly as
    /// much as one that hung up politely — but logged as what it is, because "the client
    /// went away" and "the client asked" are not the same answer to why a session ended.
    /// Every disconnection on the bus reaches this, so a name that holds nothing is
    /// `debug!` and not a line per unrelated client.
    async fn owner_gone(&self, owner: &UniqueName<'_>) {
        self.release_owner(owner, "client left the bus").await;
    }

    /// Whether a session is running right now.
    pub async fn is_running(&self) -> bool {
        self.state
            .lock()
            .await
            .session
            .as_ref()
            .is_some_and(|session| !session.task.is_finished())
    }
}

/// Watches `NameOwnerChanged` and releases a discovery reference when the bus name that
/// holds it disappears.
///
/// Spawned once, for the daemon's lifetime: without this, a client that is killed
/// (`SIGKILL`, a crash) rather than calling `StopDiscovery` would hold its reference —
/// and the unpaired scanners it never releases — forever. Only unique names are
/// tracked; a well-known name losing its owner is not a client going away in the sense
/// `start`/`release` mean (nothing here is ever called with one).
pub fn watch_owners(discovery: Arc<Discovery>, connection: Connection) -> JoinHandle<()> {
    tokio::spawn(async move {
        let dbus = match zbus::fdo::DBusProxy::new(&connection).await {
            Ok(proxy) => proxy,
            Err(error) => {
                warn!(
                    %error,
                    "could not watch NameOwnerChanged; a discovery client that crashes \
                     will not be noticed"
                );
                return;
            }
        };

        let mut changes = match dbus.receive_name_owner_changed().await {
            Ok(stream) => stream,
            Err(error) => {
                warn!(
                    %error,
                    "could not subscribe to NameOwnerChanged; a discovery client that \
                     crashes will not be noticed"
                );
                return;
            }
        };

        while let Some(signal) = changes.next().await {
            let Ok(args) = signal.args() else { continue };

            // A name still has an owner, or never did (an acquisition, not a loss).
            if args.new_owner().is_some() {
                continue;
            }

            if let BusName::Unique(name) = args.name() {
                discovery.owner_gone(name).await;
            }
        }
    })
}

/// The session task: probe, publish, wait, repeat, until stopped or out of time.
async fn run_session(
    backends: Vec<RankedBackend>,
    scanners: Arc<ScannerRegistry>,
    mut stopped: watch::Receiver<bool>,
) {
    let deadline = Instant::now() + SESSION_LIMIT;
    let probed: Vec<&'static str> = backends.iter().map(|entry| entry.backend.id()).collect();

    loop {
        let mut tasks = probe(&backends);
        let mut seen = Vec::new();
        let mut stop_requested = false;

        // Published as each backend answers rather than after the whole round — see
        // `probe`. Leaving the loop drops `tasks`, which aborts whatever is still in
        // flight.
        while let Some(result) = tasks.join_next().await {
            let (backend, found) = match result {
                Ok(batch) => batch,
                // A panicking backend takes down its own probe and nothing else.
                Err(error) => {
                    warn!(%error, "a backend probe panicked");
                    continue;
                }
            };

            // Checked before each backend's scanners rather than once per round: a
            // `StopDiscovery` that arrived while the round was in flight must not be
            // followed by an `InterfacesAdded` for a scanner it was supposed to take
            // away. Anything published before it arrived is taken away by the release
            // path, which ends the session's objects.
            if *stopped.borrow() {
                stop_requested = true;
                break;
            }

            for info in found {
                seen.push(info.id.clone());
                if let Err(error) = scanners.observe(&backend, info).await {
                    warn!(%error, "could not publish a discovered scanner");
                }
            }
        }

        if stop_requested {
            break;
        }

        // The other half of a round: what the backends stopped finding. A paired scanner
        // that has been switched off is not removed — §9 keeps it paired — it goes
        // `Status="offline"`, which is the only reachability signal this iteration has.
        scanners.mark_unseen_offline(&probed, &seen).await;

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
/// Returns the tasks rather than their results, so the caller can publish a backend's
/// scanners the moment that backend answers. Joining them all here first is what
/// [`BACKEND_TIMEOUT`]'s "only as slow as its slowest *answering* backend" is meant to
/// rule out, and did not: a backend that only ever times out held back everything the
/// others had already found, for the full 10 s, every round. `scanbus discover`'s default
/// `--for` is that same 10 s, so the batching version could publish a scanner
/// microseconds after the CLI had given up and printed an empty table.
///
/// The cost is that [`RankedBackend::rank`] no longer settles which of two backends'
/// sightings of one device is the published one. The registry keeps whichever arrives
/// first and never replaces it (see [`crate::scanners`]), and that is now finishing
/// order — so a fast generic backend can claim a device a vendor backend would have
/// claimed, along with the buttons only the vendor backend can deliver. Rank still
/// orders the scanners within one backend's answer, and is still what a filtered subset
/// preserves.
fn probe(backends: &[RankedBackend]) -> JoinSet<(RankedBackend, Vec<scanbus_core::ScannerInfo>)> {
    let mut tasks = JoinSet::new();

    for entry in backends {
        let entry = entry.clone();
        let backend: Arc<dyn ScannerBackend> = Arc::clone(&entry.backend);

        tasks.spawn(async move {
            let id = backend.id();
            let outcome = tokio::time::timeout(BACKEND_TIMEOUT, backend.discover()).await;

            let found = match outcome {
                Ok(Ok(scanners)) => {
                    debug!(backend = id, found = scanners.len(), "backend probed");
                    scanners
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
            };

            (entry, found)
        });
    }

    tasks
}
