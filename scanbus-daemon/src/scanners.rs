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
use std::sync::Arc;

use scanbus_core::{ScannerId, ScannerInfo};
use tokio::sync::Mutex;
use tracing::{debug, info, instrument, warn};

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

/// One scanner the daemon has an object for.
#[derive(Debug, Clone)]
struct Entry {
    info: ScannerInfo,
    origin: Origin,
    /// Precedence of the backend that owns this sighting; see [`crate::backends`].
    rank: usize,
    /// The deduplication key this entry claimed, kept so removal can release it.
    address: String,
}

/// The scanner objects, and the bookkeeping that decides their lifetime.
///
/// Every mutation goes through the one lock, and every publication through
/// [`ObjectRegistry`], so the map and the bus cannot drift apart.
pub struct ScannerRegistry {
    objects: Arc<ObjectRegistry>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    entries: BTreeMap<ScannerId, Entry>,
    /// Physical address → the scanner that owns it. The index that makes one device
    /// found by two backends one object.
    owners: BTreeMap<String, ScannerId>,
}

impl ScannerRegistry {
    /// A registry publishing into `objects`.
    pub fn new(objects: Arc<ObjectRegistry>) -> Self {
        Self {
            objects,
            state: Mutex::new(State::default()),
        }
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
    pub async fn observe(&self, rank: usize, info: ScannerInfo) -> Result<(), Error> {
        let mut state = self.state.lock().await;
        let address = physical_address(&info);

        if state.entries.contains_key(&info.id) {
            return self.update_existing(&mut state, rank, info, address).await;
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

        let path = path::scanner(&info.id);
        self.objects
            .add(path, Scanner1::new(info.clone(), false))
            .await?;
        info!(name = %info.name, address = %info.address, "scanner discovered");

        state.owners.insert(address.clone(), info.id.clone());
        state.entries.insert(
            info.id.clone(),
            Entry {
                info,
                origin: Origin::Discovered,
                rank,
                address,
            },
        );

        Ok(())
    }

    /// Publishes a scanner that outlives every discovery session.
    ///
    /// This is what pairing (2.3) and the restore path (4.2) call. A scanner currently
    /// present because a probe saw it is promoted in place — same object, same path,
    /// `Paired` flipped — rather than removed and re-added, so a client that is
    /// watching it never sees it disappear.
    ///
    /// # Errors
    ///
    /// [`Error::ObjectServer`] if the export failed.
    #[instrument(level = "info", skip_all, fields(id = %info.id))]
    pub async fn register_persistent(&self, info: ScannerInfo) -> Result<(), Error> {
        let mut state = self.state.lock().await;
        let address = physical_address(&info);
        let path = path::scanner(&info.id);

        if let Some(entry) = state.entries.get_mut(&info.id) {
            entry.origin = Origin::Persistent;
            entry.info = info.clone();
            entry.rank = 0;

            let iface = self.objects.interface::<Scanner1>(&path).await?;
            let emitted = async {
                scanner::update(&iface, &info).await?;
                scanner::set_paired(&iface, true).await
            };
            emitted
                .await
                .map_err(|source| Error::PropertiesChanged { path, source })?;

            info!("scanner is now paired");
            return Ok(());
        }

        self.objects
            .add(path, Scanner1::new(info.clone(), true))
            .await?;

        // A paired scanner takes the address over from whatever discovery had put
        // there: it is the object the user bound to.
        state.owners.insert(address.clone(), info.id.clone());
        state.entries.insert(
            info.id.clone(),
            Entry {
                info,
                // Rank 0: nothing outranks a paired scanner.
                rank: 0,
                origin: Origin::Persistent,
                address,
            },
        );

        Ok(())
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

        if removed > 0 {
            info!(scanners = removed, "discovery session objects removed");
        }

        removed
    }

    /// Whether this scanner has an object, and why it has one.
    pub async fn origin(&self, id: &ScannerId) -> Option<Origin> {
        self.state.lock().await.entries.get(id).map(|e| e.origin)
    }

    /// The scanners with an object right now, in path order.
    pub async fn ids(&self) -> Vec<ScannerId> {
        self.state.lock().await.entries.keys().cloned().collect()
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
            let entry = state.entries.get(&info.id).expect("checked by the caller");
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
