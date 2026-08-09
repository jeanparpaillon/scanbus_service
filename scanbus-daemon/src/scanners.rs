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

use scanbus_core::{PairingMachine, PairingStore, ScannerBackend, ScannerId, ScannerInfo, Status};
use tokio::sync::Mutex;
use tracing::{debug, info, instrument, warn};

use crate::backends::RankedBackend;
use crate::dbus::scanner::{self, Scanner1};
use crate::dbus::{ObjectRegistry, path};
use crate::error::Error;

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
    pub fn new(objects: Arc<ObjectRegistry>, store: Arc<dyn PairingStore>) -> Arc<Self> {
        Arc::new_cyclic(|self_ref| Self {
            objects,
            store,
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
            entry.origin = Origin::Persistent;
            entry.info = info.clone();
            entry.rank = 0;

            let iface = self.objects.interface::<Scanner1>(&path).await?;
            scanner::update(&iface, &info)
                .await
                .map_err(|source| Error::PropertiesChanged { path, source })?;
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
                origin: Origin::Persistent,
                address,
                discovered: false,
            },
            true,
        )
        .await
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

            if let Some(entry) = state.entries.remove(&id) {
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
            if !probed.contains(&entry.backend_id)
                || seen.contains(id)
                || entry.info.status == Status::Offline
            {
                continue;
            }

            let mut info = entry.info.clone();
            info.status = Status::Offline;

            let path = path::scanner(id);
            match self.objects.interface::<Scanner1>(&path).await {
                Ok(iface) => {
                    if let Err(error) = scanner::update(&iface, &info).await {
                        warn!(%id, %error, "could not announce that a scanner went offline");
                    }
                }
                Err(error) => {
                    warn!(%id, %error, "a tracked scanner has no object");
                    continue;
                }
            }

            info!(%id, backend = entry.backend_id, "scanner is no longer reachable");
            entry.info = info;
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

    /// Exports a new scanner object, its pairing machine and its supervisor task.
    ///
    /// `paired` is the restore path's (4.2): the machine is put in `Done` *before* the
    /// export, so the `InterfacesAdded` a client receives already says `Paired=true`
    /// rather than being corrected a moment later by a `PropertiesChanged`.
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
            backend,
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
        info!(name = %info.name, address = %info.address, paired, "scanner published");

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
        let (previous_rank, previous_address, unchanged) = {
            let entry = state
                .entries
                .get_mut(&info.id)
                .expect("checked by the caller");
            // Whatever else this round decides, the session has now seen this scanner.
            entry.discovered = true;
            (entry.rank, entry.address.clone(), entry.info == info)
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
