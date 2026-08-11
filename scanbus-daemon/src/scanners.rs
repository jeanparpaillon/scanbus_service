//! [`ScannerRegistry`]: which scanner objects exist, and why.
//!
//! §1 of [`scanbus-dbus-api.md`] gives a discovered scanner the *same* object as a
//! paired one, with `Paired=false`, and gives it a lifetime bounded by the discovery
//! session. Those two sentences together are this module: one object class, two reasons
//! for an object to exist, and `StopDiscovery` may only take away the objects that exist
//! for the first reason.
//!
//! Getting that wrong is not a subtle bug — it is a paired scanner disappearing off the
//! bus because someone ran `scanbus discover` — so the reason is stored per object
//! ([`Origin`]) rather than inferred from `Paired` at removal time. They agree today;
//! they stop agreeing the moment a pairing fails, since §9 keeps a failed scanner
//! present with `Paired=false` and that object *is* still the discovery session's.
//!
//! # Every object comes with a pairing machine and a task
//!
//! An object is not just a `Scanner1`: it is a [`PairingMachine`] for that scanner, the
//! `Scanner1` that exports it, and a [`scanner::supervise`] task turning the machine's
//! transitions into `PropertiesChanged`. All three are created here, together, because
//! all three have exactly the object's lifetime — and because `Pair()` has to work on
//! whatever a discovery session just published (§7), not only on scanners the daemon
//! decided in advance were pairable.
//!
//! # Buttons appear with the object, not with the pairing
//!
//! §5 says the `Button1` objects are "created automatically at pairing time". They are
//! created **with the scanner object** here instead, from `Capabilities.buttons.count`,
//! and that is a deliberate reading of the same sentence: 2.5 asks for them "created and
//! removed with the scanner", and they are children of its path, so anything else would
//! mean a second lifetime to get wrong. zbus destroys a node with its last interface,
//! children included ([`ObjectRegistry`]), so buttons that outlived their scanner would
//! vanish without an `InterfacesRemoved` — and buttons created later would have to be
//! added on the pairing path, removed on the `Unpair()` path that *keeps* the object
//! ([`ScannerRegistry::retire`]), and reconciled on the restore path.
//!
//! What a client loses is nothing it can act on: an unpaired scanner's keys are visible
//! and configurable, which is what a configuration UI wants anyway, and a `Profile` write
//! reaches the same backend either way. A scanner reporting `buttons.count=0` gets no
//! button objects at all, which is not an error — a plain flatbed has no walk-up keys.
//!
//! # And, once it is paired, a listener
//!
//! A paired scanner also gets a **listener task**, at most one, keyed by its
//! [`ScannerId`] and stored in its [`Entry`]. `Connect()`, the end of a pairing and the
//! restore path at startup (4.2) all reach it through
//! [`ScannerRegistry::ensure_listening`] rather than starting one of their own, which is
//! what makes `Connect()` idempotent and stops a `Disconnect()` after a re-pair from
//! dropping a stream a second task is still consuming. [`crate::listeners`] argues that
//! decision and owns the task's body; what is here is its lifetime.
//!
//! # Deduplication
//!
//! [`Backends`](crate::backends) fixes which backend outranks which. This module
//! applies it, keyed by [`physical_address`], with two rules that are not in the
//! precedence order itself:
//!
//! - **A paired scanner always wins**, whatever found it. It is the object the user
//!   bound to and possibly the one a `Button1` mapping points at.
//! - **An object already published is never replaced**, even by a better-ranked
//!   sighting arriving in a later probe round. A client may already be acting on the
//!   path it saw — a `Pair()` can be in flight — and swapping the object underneath it
//!   would turn a rediscovery into an `UnknownObject`. Precedence therefore settles
//!   ties *within* a round, where nothing has been published yet, which is the case §9
//!   is about: one device, two backends, one probe.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use scanbus_core::{
    ButtonInfo, PairingMachine, PairingStore, ProfileKind, Restorable, ScannerBackend,
    ScannerId, ScannerInfo, Status,
};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, info, instrument, warn};

use crate::backends::{Backends, RankedBackend};
use crate::dbus::button::Button1;
use crate::dbus::scanner::{self, Scanner1};
use crate::dbus::{ObjectRegistry, path};
use crate::error::Error;
use crate::jobs::JobRegistry;
use crate::listeners::{self, ButtonEventSink, ListenError, RestartPolicy};
use crate::profiles::ProfileRegistry;

mod address;

pub use address::physical_address;

/// Why a scanner object exists — and therefore when it may be removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A probe saw it during the current discovery session. Removed when that session
    /// ends, however it ends.
    Discovered,
    /// It is paired with this host, or restored from the pairing store (4.2). Survives
    /// every discovery session, including the ones that do not see it at all.
    Persistent,
}

/// What [`ScannerRegistry::retire`] did with a scanner that has just been unpaired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retired {
    /// The object went away, with an `InterfacesRemoved` for it and its children.
    Removed,
    /// The object stayed: the current discovery session has seen this scanner, so it
    /// goes back to being a discovered one with `Paired=false` (§1).
    Kept,
    /// No such scanner. Reachable when two `Unpair()` calls race, or when a discovery
    /// session ended between the call and this.
    Unknown,
}

/// One scanner the daemon has an object for.
struct Entry {
    info: ScannerInfo,
    origin: Origin,
    /// The backend this sighting came from.
    ///
    /// Kept as the object rather than only as its id, because the listener has to be
    /// started against the same backend that found the device — not against whatever the
    /// daemon's list happens to rank first when `Connect()` arrives.
    backend: Arc<dyn ScannerBackend>,
    /// The listener task, while one is running. `Connected` is exactly `Some`.
    ///
    /// Held here rather than in a map of its own so that it cannot outlive the object it
    /// belongs to: every path that drops an [`Entry`] drops this with it, and the ones
    /// that mean to keep the object ([`ScannerRegistry::stop_listening`]) take it out
    /// deliberately.
    listener: Option<JoinHandle<()>>,
    /// Precedence of the backend that owns this sighting; see [`crate::backends`].
    rank: usize,
    /// Id of that backend, as [`ScannerBackend::id`] reports it.
    ///
    /// Kept because reachability is per backend: a round that did not probe this
    /// scanner's backend has said nothing about whether it is still there
    /// ([`ScannerRegistry::mark_unseen_offline`]).
    backend_id: &'static str,
    /// The deduplication key this entry claimed, kept so removal can release it.
    address: String,
    /// Whether the *current* discovery session has seen this scanner.
    ///
    /// Distinct from [`Origin`], which says why the object exists: a paired scanner a
    /// probe is also finding is `Persistent` *and* seen, and that combination is exactly
    /// what decides whether `Unpair()` may take its object away.
    discovered: bool,
}

/// The scanner objects, and the bookkeeping that decides their lifetime.
///
/// Every mutation goes through the one lock, and every publication through
/// [`ObjectRegistry`], so the map and the bus cannot drift apart.
///
/// **The lock is held across `PropertiesChanged` emissions**, i.e. across an interface
/// lock. That is safe only because no `Scanner1` method takes `&mut self` and nothing
/// calls [`get_mut`](zbus::object_server::InterfaceRef::get_mut) on one, so those
/// interface locks are read locks that never wait; [`crate::dbus::scanner`] documents the
/// cycle that appears the moment that stops being true.
pub struct ScannerRegistry {
    objects: Arc<ObjectRegistry>,
    store: Arc<dyn PairingStore>,
    /// What a button press is turned into — [`JobRegistry`], which publishes the `Job1`.
    sink: Arc<dyn ButtonEventSink>,
    /// How hard a listener tries to come back before the scanner is declared in error.
    restart: RestartPolicy,
    state: Mutex<State>,
    /// A handle to ourselves, for the pieces that have to call back in: `Scanner1` for
    /// `Unpair()`, the supervisor task for the promotion that follows a pairing.
    ///
    /// Weak, because both of those are reachable *from* the object tree this registry
    /// publishes into — a strong handle would be a cycle through the connection.
    self_ref: Weak<Self>,
}

#[derive(Default)]
struct State {
    entries: BTreeMap<ScannerId, Entry>,
    /// Physical address → the scanner that owns it. The index that makes one device
    /// found by two backends one object.
    owners: BTreeMap<String, ScannerId>,
}

impl ScannerRegistry {
    /// A registry publishing into `objects` and making pairings durable through `store`.
    ///
    /// Returns an [`Arc`] rather than a bare `Self` because every object it publishes
    /// gets a weak handle back to it; see [`ScannerRegistry::self_ref`].
    pub fn new(
        objects: Arc<ObjectRegistry>,
        store: Arc<dyn PairingStore>,
        profiles: Arc<ProfileRegistry>,
    ) -> Arc<Self> {
        let jobs = Arc::new(JobRegistry::new(Arc::clone(&objects), profiles));
        Self::with_listeners(objects, store, jobs, RestartPolicy::DEFAULT)
    }

    /// The same registry, with the listener half configured.
    ///
    /// The two parameters exist for the two things a test needs and a daemon does not: a
    /// [`ButtonEventSink`] it can assert on — its own, or a
    /// [`JobRegistry`](crate::jobs::JobRegistry) with a retention window short enough to
    /// watch — and a [`RestartPolicy`] short enough that the exhausted case is observable
    /// without putting the daemon's real budget into the suite.
    pub fn with_listeners(
        objects: Arc<ObjectRegistry>,
        store: Arc<dyn PairingStore>,
        sink: Arc<dyn ButtonEventSink>,
        restart: RestartPolicy,
    ) -> Arc<Self> {
        Arc::new_cyclic(|self_ref| Self {
            objects,
            store,
            sink,
            restart,
            state: Mutex::new(State::default()),
            self_ref: self_ref.clone(),
        })
    }

    /// Records a sighting from a discovery probe.
    ///
    /// Three outcomes, all of them normal:
    ///
    /// - a scanner already known by [`ScannerInfo::id`] has its object updated in place
    ///   ([`scanner::update`]), whatever its origin — this is the "a paired scanner
    ///   rediscovered updates the existing object" rule of §1;
    /// - a scanner whose physical address another object already owns is dropped, with
    ///   a log line naming both backends;
    /// - anything else becomes a new object with `Paired=false`, which is what a client
    ///   sees as `InterfacesAdded`.
    ///
    /// # Errors
    ///
    /// [`Error::ObjectServer`] if the export failed. The sighting is then not recorded,
    /// so the next probe round retries it.
    #[instrument(level = "debug", skip_all, fields(id = %info.id, backend = %info.backend))]
    pub async fn observe(&self, backend: &RankedBackend, info: ScannerInfo) -> Result<(), Error> {
        let mut state = self.state.lock().await;
        let address = physical_address(&info);

        if state.entries.contains_key(&info.id) {
            return self
                .update_existing(&mut state, backend.rank, info, address)
                .await;
        }

        if let Some(owner) = state.owners.get(&address) {
            // `owner == info.id` is impossible here: the entry lookup above would have
            // caught it. So this is genuinely the same device seen twice.
            let winner = &state.entries[owner];
            debug!(
                address = %address,
                winner = %winner.info.backend,
                loser = %info.backend,
                "deduplicated: this device already has an object"
            );
            return Ok(());
        }

        self.publish(
            &mut state,
            Arc::clone(&backend.backend),
            info.clone(),
            Entry {
                info,
                origin: Origin::Discovered,
                backend: Arc::clone(&backend.backend),
                listener: None,
                rank: backend.rank,
                backend_id: backend.backend.id(),
                address,
                discovered: true,
            },
            false,
        )
        .await?;

        Ok(())
    }

    /// Publishes a scanner that outlives every discovery session.
    ///
    /// This is what the restore path (4.2) calls. A scanner currently present because a
    /// probe saw it is promoted in place — same object, same path, `Paired` flipped —
    /// rather than removed and re-added, so a client that is watching it never sees it
    /// disappear.
    ///
    /// Pairing does *not* go through here: `Pair()` runs the machine, and the promotion
    /// that follows a successful run is [`ScannerRegistry::promote`], which leaves the
    /// object and its machine exactly where they are.
    ///
    /// # Errors
    ///
    /// [`Error::ObjectServer`] if the export failed.
    #[instrument(level = "info", skip_all, fields(id = %info.id))]
    pub async fn register_persistent(
        &self,
        backend: Arc<dyn ScannerBackend>,
        info: ScannerInfo,
    ) -> Result<(), Error> {
        let mut state = self.state.lock().await;
        let address = physical_address(&info);
        let path = path::scanner(&info.id);

        if let Some(entry) = state.entries.get_mut(&info.id) {
            let previous_buttons = entry.info.capabilities.button_count();
            let published_by = Arc::clone(&entry.backend);
            entry.origin = Origin::Persistent;
            entry.info = info.clone();
            entry.rank = 0;

            let iface = self.objects.interface::<Scanner1>(&path).await?;
            scanner::update(&iface, &info)
                .await
                .map_err(|source| Error::PropertiesChanged { path, source })?;
            // The backend that published the object, not the one restoring it: the keys
            // already exported were built against the first, and a `Profile` write has to
            // keep reaching the config the listener is running from.
            self.sync_buttons(&published_by, previous_buttons, &info)
                .await;
            // The supervisor turns this into the `Paired`/`PairingState` signals; doing
            // it here as well would be the second writer this design does not have.
            iface.get().await.machine().restore_paired();

            info!("scanner is now paired");
            return Ok(());
        }

        // A paired scanner takes the address over from whatever discovery had put
        // there: it is the object the user bound to.
        self.publish(
            &mut state,
            backend.clone(),
            info.clone(),
            Entry {
                info,
                // Rank 0: nothing outranks a paired scanner.
                rank: 0,
                backend_id: backend.id(),
                backend: Arc::clone(&backend),
                listener: None,
                origin: Origin::Persistent,
                address,
                discovered: false,
            },
            true,
        )
        .await
    }

    /// Restores every persisted scanner before `org.scanbus` is requested (4.2).
    ///
    /// For each entry the store reports: publish its object (as
    /// [`register_persistent`](Self::register_persistent) does), then, if it was
    /// `Connected` when the daemon last recorded it, restart its listener through
    /// [`ensure_listening`](Self::ensure_listening) — the same call `Connect()` uses, so
    /// this is not a parallel path with its own bugs.
    ///
    /// Bounded per scanner by `per_scanner_timeout`, so one scanner whose backend hangs
    /// on `start_listening` — the switched-off case is exactly this, for a backend that
    /// blocks instead of erroring — cannot stall every scanner behind it, and cannot
    /// delay `org.scanbus` appearing on the bus at all: a scanner that times out is
    /// published anyway, with `Status="offline"` and `Connected=false`, left for the
    /// reconnect backoff ([`crate::listeners`]) to pick up once it comes back.
    ///
    /// A persisted scanner naming a backend this binary was not built with is logged and
    /// skipped: there is no [`ScannerBackend`] to hand its object, so there is nothing to
    /// publish for it.
    #[instrument(level = "info", skip_all)]
    pub async fn restore(&self, backends: &Backends, per_scanner_timeout: Duration) {
        let entries = match self.store.restorable().await {
            Ok(entries) => entries,
            Err(error) => {
                warn!(%error, "could not read the pairing store; starting with no restored scanner");
                return;
            }
        };

        for entry in entries {
            self.restore_one(backends, entry, per_scanner_timeout).await;
        }
    }

    /// One scanner's share of [`Self::restore`].
    async fn restore_one(&self, backends: &Backends, entry: Restorable, timeout: Duration) {
        let id = entry.scanner.id.clone();

        let Some(backend) = backends.get(&entry.scanner.backend) else {
            warn!(
                %id,
                backend = %entry.scanner.backend,
                "restored scanner names a backend this daemon was not built with; skipping"
            );
            return;
        };

        if let Err(error) = self
            .register_persistent(Arc::clone(&backend), entry.scanner.clone())
            .await
        {
            warn!(%id, %error, "could not publish a restored scanner");
            return;
        }

        if !entry.connected {
            info!(%id, "restored: was not connected, listener left stopped");
            return;
        }

        match tokio::time::timeout(timeout, self.ensure_listening(&id)).await {
            Ok(Ok(())) => info!(%id, "restored: listening for button presses"),
            Ok(Err(error)) => {
                self.mark_restore_offline(&id).await;
                warn!(%id, %error, "restored offline: the backend refused to listen");
            }
            Err(_) => {
                self.mark_restore_offline(&id).await;
                warn!(
                    %id,
                    timeout = ?timeout,
                    "restored offline: the backend did not respond in time"
                );
            }
        }
    }

    /// Moves a restored scanner to `Status="offline"` and announces `Connected=false`,
    /// without touching its pairing — the §9 rule [`mark_unseen_offline`] applies at
    /// startup instead of at the next probe round.
    ///
    /// [`mark_unseen_offline`]: Self::mark_unseen_offline
    async fn mark_restore_offline(&self, id: &ScannerId) {
        {
            let mut state = self.state.lock().await;
            if let Some(entry) = state.entries.get_mut(id) {
                set_status(&self.objects, entry, Status::Offline).await;
            }
        }
        self.announce_connected(id, false).await;
    }

    /// Records that a pairing succeeded, so the object stops belonging to the session.
    ///
    /// Called by [`scanner::supervise`] on the transition to `Done`, and by nothing else.
    /// It changes no property: `Paired` and `PairingState` are the machine's, and the
    /// supervisor announces them itself.
    #[instrument(level = "info", skip_all, fields(id = %id))]
    pub async fn promote(&self, id: &ScannerId) {
        let mut state = self.state.lock().await;

        let Some(entry) = state.entries.get_mut(id) else {
            warn!("paired a scanner the registry does not know");
            return;
        };

        entry.origin = Origin::Persistent;
        entry.rank = 0;
        info!("scanner is now paired");
    }

    /// Makes sure a listener task is running for this scanner, starting one if not.
    ///
    /// The single entry point of §3's `Connect()`, of the end of a pairing and of the
    /// restore path at startup (4.2) — see the module documentation on why there is only
    /// one. Calling it on a scanner that is already listening is the whole point: it
    /// returns `Ok` and starts nothing, which is what makes `Connect()` idempotent.
    ///
    /// The *first* stream is opened here, awaited, so that a `Connect()` which cannot
    /// listen at all comes back as an error reply rather than as a log line in a task the
    /// client is not watching. Everything after that — the restarts — belongs to the task
    /// ([`crate::listeners::run`]).
    ///
    /// # Errors
    ///
    /// [`ListenError::Unknown`] for a scanner with no object, [`ListenError::Backend`]
    /// for a backend that refused to arm the device. Nothing is recorded in either case,
    /// so a retry is a plain second call.
    #[instrument(level = "info", skip_all, fields(id = %id))]
    pub async fn ensure_listening(&self, id: &ScannerId) -> Result<(), ListenError> {
        // Held across `start_listening`, which is the point: two `Connect()` calls racing
        // must not both get past the "is one already running" check and open two streams
        // for one device. The cost is that a backend which is slow to arm delays the
        // other registry operations, which is the right trade for a call that is
        // supposed to be a fast handshake.
        let mut state = self.state.lock().await;

        let Some(entry) = state.entries.get(id) else {
            return Err(ListenError::Unknown(id.clone()));
        };
        if entry
            .listener
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            debug!("a listener is already running; leaving it alone");
            return Ok(());
        }

        let backend = Arc::clone(&entry.backend);
        let info = entry.info.clone();
        let stream = backend.start_listening(&info).await?;

        let task = tokio::spawn(listeners::run(
            self.self_ref.clone(),
            Arc::clone(&self.objects),
            Arc::clone(&backend),
            info,
            Arc::clone(&self.sink),
            self.restart,
            stream,
        ));

        state
            .entries
            .get_mut(id)
            .expect("the entry was there a moment ago, under this lock")
            .listener = Some(task);

        // Under the lock, like every other property this registry writes: the interface
        // is only ever read-locked, so this waits for nothing (see [`scanner`]).
        self.announce_connected(id, true).await;
        info!("listening for button presses");

        Ok(())
    }

    /// Stops the listener task, if there is one, and tells the backend to stand down.
    ///
    /// `Disconnect()`, and the first thing `Unpair()` does. Returns whether a task was
    /// actually running, which is what lets a failed `Unpair()` put back the listener it
    /// took away.
    ///
    /// The task is aborted **and awaited** before the backend is asked to stop: the task
    /// holds the stream, and a backend tearing its listener down under a live consumer
    /// looks to that consumer exactly like the device going away — which would have it
    /// start reconnecting to a scanner the user has just disconnected.
    ///
    /// Succeeds silently on a scanner that is not listening, and on one the registry does
    /// not know: §3 gives `Disconnect()` no error, and a client racing a listener that had
    /// already given up should not have to tell that apart from a real failure.
    #[instrument(level = "info", skip_all, fields(id = %id))]
    pub async fn stop_listening(&self, id: &ScannerId) -> bool {
        let mut state = self.state.lock().await;

        let Some(entry) = state.entries.get_mut(id) else {
            debug!("nothing to stop: no such scanner");
            return false;
        };

        let was_listening = match entry.listener.take() {
            Some(task) => {
                let running = !task.is_finished();
                task.abort();
                // Awaited so that the stream is provably dropped before the backend is
                // asked to stop. An aborted task resolves at once, and one parked on this
                // very lock is cancelled at that await rather than deadlocking on it.
                let _ = task.await;
                running
            }
            None => false,
        };

        if let Err(error) = entry.backend.stop_listening(id).await {
            // Logged, not raised: `Disconnect()` cannot fail, and a vendor daemon that
            // will not shut down must not leave a client believing it is still connected.
            warn!(%error, "the backend could not stop listening");
        }

        self.announce_connected(id, false).await;
        if was_listening {
            info!("no longer listening");
        }

        was_listening
    }

    /// Whether a listener task is running for this scanner.
    pub async fn is_listening(&self, id: &ScannerId) -> bool {
        self.state
            .lock()
            .await
            .entries
            .get(id)
            .is_some_and(|entry| {
                entry
                    .listener
                    .as_ref()
                    .is_some_and(|task| !task.is_finished())
            })
    }

    /// Records that a listener task gave up: `Status="error"`, `Connected=false`.
    ///
    /// Called by the task itself, from [`crate::listeners::run`], once its restart budget
    /// is exhausted — and by nothing else. It is the other half of the reason the budget
    /// is bounded: a scanner that will never deliver another press has to say so where a
    /// client can see it, or the only symptom is a key that silently does nothing.
    ///
    /// The pairing is untouched. §9's "reachable ≠ paired" applies here as much as it
    /// does to a probe that stopped finding a device: the association, the mappings and
    /// the object all survive, and a `Connect()` retries from a clean start.
    pub(crate) async fn listener_failed(&self, id: &ScannerId) {
        let mut state = self.state.lock().await;

        let Some(entry) = state.entries.get_mut(id) else {
            return;
        };
        // Dropping our own `JoinHandle`, which detaches rather than aborts — the task is
        // returning as soon as this call comes back.
        entry.listener = None;
        let moved = set_status(&self.objects, entry, Status::Error).await;

        drop(state);
        self.announce_connected(id, false).await;

        if moved {
            warn!(%id, "scanner is in error: its button stream could not be restored");
        }
    }

    /// Stops every listener, so nothing is consuming a backend stream while the object
    /// tree comes down.
    ///
    /// The shutdown counterpart of [`ObjectRegistry::shutdown`], and run before it:
    /// unexporting an object a listener task is about to look up is how the last seconds
    /// of a daemon's life fill with warnings about objects that are gone. No
    /// `PropertiesChanged` for `Connected` — the objects themselves are about to go, and a
    /// client learns from `InterfacesRemoved`.
    #[instrument(level = "info", skip_all)]
    pub async fn shutdown(&self) {
        let mut state = self.state.lock().await;
        let mut stopped = 0;

        for (id, entry) in &mut state.entries {
            let Some(task) = entry.listener.take() else {
                continue;
            };
            task.abort();
            let _ = task.await;

            if let Err(error) = entry.backend.stop_listening(id).await {
                warn!(%id, %error, "the backend could not stop listening on shutdown");
            }
            stopped += 1;
        }

        if stopped > 0 {
            info!(listeners = stopped, "listeners stopped");
        }
    }

    /// Retires a scanner that has just been unpaired.
    ///
    /// §1 decides this, not `Paired`: an object exists either because a probe saw it or
    /// because it is paired, and unpairing removes only the second reason. A scanner the
    /// current session has also seen therefore keeps its object as a plain discovered
    /// one — which is what lets a user unpair and immediately re-pair from the same path
    /// — and one whose only reason to exist was the pairing goes away.
    pub async fn retire(&self, id: &ScannerId) -> Retired {
        let mut state = self.state.lock().await;

        let Some(entry) = state.entries.get_mut(id) else {
            return Retired::Unknown;
        };

        // Belt and braces: `Unpair()` stops the listener before it gets here, but an
        // object that stays as a discovered one must not keep a task that a `Pair()` from
        // the same path would then find "already running".
        if let Some(task) = entry.listener.take() {
            task.abort();
        }

        if entry.discovered {
            entry.origin = Origin::Discovered;
            debug!(%id, "unpaired scanner kept: the discovery session still sees it");
            return Retired::Kept;
        }

        let entry = state.entries.remove(id).expect("looked up just above");
        state.owners.remove(&entry.address);

        // The subtree, not the object: 2.5 hangs `Button1` children off a scanner, and
        // zbus would drop them silently along with their parent.
        let path = path::scanner(id);
        if let Err(error) = self.objects.remove_subtree(&path).await {
            // Dropped from the map anyway: a registry that keeps retrying an object the
            // bus may no longer have is the drift [`ObjectRegistry`] exists to prevent.
            warn!(%id, %error, "could not remove an unpaired scanner");
        }

        info!(%id, "unpaired scanner removed");
        Retired::Removed
    }

    /// Removes every object that exists only because a probe saw it.
    ///
    /// Called by `StopDiscovery` and by a session that ended on its own; idempotent, so
    /// the two racing is harmless. Returns how many objects went away.
    ///
    /// A failed removal is logged and the entry dropped anyway: the alternative is a
    /// registry that keeps retrying an object the bus may no longer have, which is the
    /// drift [`ObjectRegistry`] exists to prevent.
    pub async fn end_discovery(&self) -> usize {
        let mut state = self.state.lock().await;

        let transient: Vec<ScannerId> = state
            .entries
            .iter()
            .filter(|(_, entry)| entry.origin == Origin::Discovered)
            .map(|(id, _)| id.clone())
            .collect();

        let mut removed = 0;
        for id in transient {
            let path = path::scanner(&id);
            // The subtree, not the object: 2.5 hangs `Button1` children off a scanner,
            // and zbus would drop them silently along with their parent.
            match self.objects.remove_subtree(&path).await {
                Ok(()) => removed += 1,
                Err(error) => warn!(%id, %error, "could not remove a discovered scanner"),
            }

            if let Some(mut entry) = state.entries.remove(&id) {
                // A discovered scanner is by definition unpaired and so has no listener;
                // taking it anyway means no path can drop an `Entry` and leave a task
                // reading a stream for an object that no longer exists.
                if let Some(task) = entry.listener.take() {
                    task.abort();
                }
                state.owners.remove(&entry.address);
            }
        }

        // The survivors are the paired ones, and no session has seen them any more:
        // whether the *next* session does is what decides where a later `Unpair()`
        // leaves their object.
        for entry in state.entries.values_mut() {
            entry.discovered = false;
        }

        if removed > 0 {
            info!(scanners = removed, "discovery session objects removed");
        }

        removed
    }

    /// Moves every scanner a probe round should have found, and did not, to
    /// `Status="offline"`.
    ///
    /// This is where §9's "reachable ≠ paired" gets its teeth: `Paired` is untouched, so
    /// a scanner that is switched off keeps its pairing, its object and its button
    /// mappings and simply stops being reachable. The signal is the probe round itself,
    /// because a backend's `discover()` is the only thing in this iteration that has an
    /// opinion on whether a device is still there — 2.4's listener is the other one, and
    /// it will report the same property from the other end.
    ///
    /// `probed` is what the round actually asked. A `StartDiscovery` restricted with
    /// `{"backends": …}` says nothing about the scanners of the backends it skipped, and
    /// marking those offline would turn a client's filter into a lie about the hardware.
    pub async fn mark_unseen_offline(&self, probed: &[&'static str], seen: &[ScannerId]) {
        let mut state = self.state.lock().await;

        for (id, entry) in &mut state.entries {
            if !probed.contains(&entry.backend_id) || seen.contains(id) {
                continue;
            }

            if set_status(&self.objects, entry, Status::Offline).await {
                info!(%id, backend = entry.backend_id, "scanner is no longer reachable");
            }
        }
    }

    /// Whether this scanner has an object, and why it has one.
    pub async fn origin(&self, id: &ScannerId) -> Option<Origin> {
        self.state.lock().await.entries.get(id).map(|e| e.origin)
    }

    /// The scanners with an object right now, in path order.
    pub async fn ids(&self) -> Vec<ScannerId> {
        self.state.lock().await.entries.keys().cloned().collect()
    }

    /// Persists `DefaultProfile` for this scanner when the store supports it.
    pub async fn persist_default_profile(&self, id: &ScannerId, profile: Option<ProfileKind>) {
        if let Err(error) = self.store.save_default_profile(id, profile).await {
            warn!(%id, %error, "could not persist DefaultProfile");
        }
    }

    /// Persists one `Button1` assignment (`Label`, `Profile`, `ProfileOptions`).
    pub async fn persist_button_assignment(&self, id: &ScannerId, button: &ButtonInfo) {
        if let Err(error) = self.store.save_button(id, button).await {
            warn!(%id, index = button.index, %error, "could not persist button assignment");
        }
    }

    /// Writes `Connected` on a scanner's object, if it still has one, and persists it —
    /// what the restore path (4.2) reads back to decide whether to listen again.
    ///
    /// Best effort by design: the listener task is the truth, and an object that has gone
    /// away between the task starting and this call has no client left to tell.
    async fn announce_connected(&self, id: &ScannerId, connected: bool) {
        let path = path::scanner(id);

        match self.objects.interface::<Scanner1>(&path).await {
            Ok(iface) => {
                if let Err(error) = scanner::set_connected(&iface, connected).await {
                    warn!(%id, %error, "could not announce a change of Connected");
                }
            }
            Err(error) => debug!(%id, %error, "no object to announce Connected on"),
        }

        if let Err(error) = self.store.save_connected(id, connected).await {
            warn!(%id, %error, "could not persist Connected");
        }
    }

    /// Exports a new scanner object, its pairing machine and its supervisor task.
    ///
    /// `paired` is the restore path's (4.2): the machine is put in `Done` *before* the
    /// export, so the `InterfacesAdded` a client receives already says `Paired=true`
    /// rather than being corrected a moment later by a `PropertiesChanged`.
    ///
    /// The `Button1` children go up with it, in index order, so a client watching
    /// `InterfacesAdded` sees the scanner before the keys that hang off it.
    async fn publish(
        &self,
        state: &mut State,
        backend: Arc<dyn ScannerBackend>,
        info: ScannerInfo,
        entry: Entry,
        paired: bool,
    ) -> Result<(), Error> {
        let path = path::scanner(&info.id);
        let machine = Arc::new(PairingMachine::new(
            info.clone(),
            Arc::clone(&backend),
            Arc::clone(&self.store),
        ));
        if paired {
            machine.restore_paired();
        }

        // Subscribed before the state is read, so the supervisor's starting point cannot
        // be a transition it also receives, nor one it never hears about.
        let transitions = machine.subscribe();
        let initial = machine.state();

        self.objects
            .add(
                path.clone(),
                Scanner1::new(Arc::clone(&machine), self.self_ref.clone()),
            )
            .await?;

        // Rolled back rather than left half-published: an object the bus serves and this
        // registry does not track is exactly the drift [`ObjectRegistry`] exists to
        // prevent, and the caller retries the whole sighting on the next probe round.
        if let Err(error) = self.add_buttons(&backend, &info, 0).await {
            if let Err(cleanup) = self.objects.remove_subtree(&path).await {
                warn!(id = %info.id, %cleanup, "could not roll back a half-published scanner");
            }
            return Err(error);
        }

        info!(
            name = %info.name,
            address = %info.address,
            buttons = info.capabilities.button_count(),
            paired,
            "scanner published"
        );

        tokio::spawn(scanner::supervise(
            Arc::clone(&self.objects),
            self.self_ref.clone(),
            path,
            info.id.clone(),
            initial,
            transitions,
        ));

        state.owners.insert(entry.address.clone(), info.id.clone());
        state.entries.insert(info.id, entry);

        Ok(())
    }

    /// Exports the `Button1` objects of `info` from index `exported` upward.
    ///
    /// `exported` is how many this scanner already has, so a fresh publication passes `0`
    /// and a device that grew a key passes the count it had. The ones already up are left
    /// alone on purpose: they carry the host's assignments, and re-exporting them would
    /// throw away a mapping because a probe round re-read the same capabilities.
    ///
    /// # Errors
    ///
    /// [`Error::ObjectServer`] for the first export zbus refused.
    async fn add_buttons(
        &self,
        backend: &Arc<dyn ScannerBackend>,
        info: &ScannerInfo,
        exported: u32,
    ) -> Result<(), Error> {
        for button in ButtonInfo::from_capabilities(&info.capabilities)
            .into_iter()
            .skip(exported as usize)
        {
            let path = path::button(&info.id, button.index);
            self.objects
                .add(
                    path,
                    Button1::new(
                        info.id.clone(),
                        Arc::clone(backend),
                        Arc::clone(&self.store),
                        button,
                    ),
                )
                .await?;
        }

        Ok(())
    }

    /// Unexports the `Button1` objects of `id` in `from..to`, highest index first.
    ///
    /// A failure is logged and the rest still removed: leaving four keys on the bus
    /// because the third refused to go is worse than a device that reports three.
    async fn remove_buttons(&self, id: &ScannerId, from: u32, to: u32) {
        for index in (from..to).rev() {
            let path = path::button(id, index);
            if let Err(error) = self.objects.remove_object(&path).await {
                warn!(%id, index, %error, "could not remove a button object");
            }
        }
    }

    /// Brings the `Button1` objects in line with a freshly reported `buttons.count`.
    ///
    /// A device does not usually grow keys, but a backend can revise what it reports —
    /// a first probe that could not reach the device, a vendor tool that learnt the model
    /// on the second look. The object tree has to follow, or a client reads
    /// `Capabilities.buttons.count=4` and finds three objects under `…/button`.
    ///
    /// Keys that survive keep their assignments: only the tail moves.
    async fn sync_buttons(
        &self,
        backend: &Arc<dyn ScannerBackend>,
        previous: u32,
        info: &ScannerInfo,
    ) {
        let desired = info.capabilities.button_count();
        if desired == previous {
            return;
        }

        if desired > previous {
            if let Err(error) = self.add_buttons(backend, info, previous).await {
                warn!(id = %info.id, %error, "could not publish the new button objects");
            }
        } else {
            self.remove_buttons(&info.id, desired, previous).await;
        }

        info!(id = %info.id, from = previous, to = desired, "the physical menu changed size");
    }

    /// Updates the object of a scanner already known by id.
    ///
    /// The rank is *not* revised: a device that a better-ranked backend also found
    /// keeps the object it already has (see the module documentation), and a rank that
    /// changed under an existing object would only make the log misleading.
    async fn update_existing(
        &self,
        state: &mut State,
        rank: usize,
        info: ScannerInfo,
        address: String,
    ) -> Result<(), Error> {
        let (previous_rank, previous_address, previous_buttons, backend, unchanged) = {
            let entry = state
                .entries
                .get_mut(&info.id)
                .expect("checked by the caller");
            // Whatever else this round decides, the session has now seen this scanner.
            entry.discovered = true;
            (
                entry.rank,
                entry.address.clone(),
                entry.info.capabilities.button_count(),
                Arc::clone(&entry.backend),
                entry.info == info,
            )
        };

        if previous_rank != rank {
            debug!(
                kept = previous_rank,
                seen = rank,
                "the same scanner id was reported by another backend"
            );
        }

        if unchanged {
            return Ok(());
        }

        let path = path::scanner(&info.id);
        let iface = self.objects.interface::<Scanner1>(&path).await?;
        scanner::update(&iface, &info)
            .await
            .map_err(|source| Error::PropertiesChanged {
                path: path.clone(),
                source,
            })?;

        // After the property change, so a client that reacts to `Capabilities` by walking
        // `…/button` finds the tree the new count describes.
        self.sync_buttons(&backend, previous_buttons, &info).await;

        if previous_address != address {
            debug!(from = %previous_address, to = %address, "physical address moved");
            state.owners.remove(&previous_address);
            state.owners.insert(address.clone(), info.id.clone());
        }

        let entry = state
            .entries
            .get_mut(&info.id)
            .expect("checked by the caller");
        entry.address = address;
        entry.info = info;

        Ok(())
    }
}

/// Moves one scanner's `Status`, announcing it, and says whether anything moved.
///
/// The two things that have an opinion on reachability write it through here: a probe
/// round that stopped finding a device ([`ScannerRegistry::mark_unseen_offline`]) and a
/// listener that could not be restored ([`ScannerRegistry::listener_failed`]). Both end
/// in the same place — the entry and the object agreeing — and neither should have to
/// spell out the `PropertiesChanged`.
///
/// A free function rather than a method because both callers hold a `&mut Entry` borrowed
/// out of the state map, and a `&self` method would want the map back.
async fn set_status(objects: &ObjectRegistry, entry: &mut Entry, status: Status) -> bool {
    if entry.info.status == status {
        return false;
    }

    let mut info = entry.info.clone();
    info.status = status;

    let path = path::scanner(&info.id);
    match objects.interface::<Scanner1>(&path).await {
        Ok(iface) => {
            if let Err(error) = scanner::update(&iface, &info).await {
                // Announced or not, the daemon's own view has to move: a status it keeps
                // retrying to publish is a scanner that never comes back online either.
                warn!(id = %info.id, %error, "could not announce a status change");
            }
        }
        Err(error) => {
            warn!(id = %info.id, %error, "a tracked scanner has no object");
            return false;
        }
    }

    entry.info = info;
    true
}
