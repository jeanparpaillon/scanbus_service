//! [`ObjectRegistry`]: the one thing allowed to touch zbus's object server.
//!
//! §1 of [`scanbus-dbus-api.md`] makes the service an `ObjectManager` and its scanners,
//! buttons and jobs *objects*, not values returned from calls. That is what removes the
//! need for a `ScannerFound` signal or a `ListJobs` method (§2): a client subscribes to
//! `InterfacesAdded`/`InterfacesRemoved` once and sees everything, including the jobs
//! already in flight when it connected.
//!
//! The price is that `GetManagedObjects` and the signals have to agree forever. They
//! are both derived by zbus from the object server's contents, so they cannot disagree
//! with *each other* — what drifts is the daemon's own state against the object server,
//! the first time an error path forgets an unexport. The symptom is a client that sees
//! an object and gets `UnknownObject` when it calls one, three issues away from the
//! code that caused it.
//!
//! So exporting is centralised here. Adding a scanner to the registry is what publishes
//! the object; nothing else in the daemon calls
//! [`Connection::object_server`](zbus::Connection::object_server), and
//! [`ObjectRegistry::shutdown`] can therefore take the whole tree down knowing what is
//! in it.
//!
//! # Two things zbus's object server does that the tree of §1 runs into
//!
//! - **A node dies with its last interface, children included.** Unexporting
//!   `Scanner1` from `/org/scanbus/scanner/{id}` drops that node from the object
//!   server, and its `button/{n}` and `job/{jobid}` children go with it — silently,
//!   with no `InterfacesRemoved` for any of them. That is why
//!   [`ObjectRegistry::remove_object`] refuses a path that still has exported
//!   descendants and [`ObjectRegistry::remove_subtree`] removes children first.
//! - **`GetManagedObjects` lists the structural nodes too.** `/org/scanbus/scanner`
//!   and `/org/scanbus/scanner/{id}/button` are path elements that carry no interface,
//!   and they come back in the dict with an empty interface map. Nothing is ever
//!   *added* or *removed* for them — the signals only ever mention objects that have
//!   an interface — so a client that keys on `org.scanbus.Scanner1` never notices. One
//!   that iterates the dict has to ignore entries with no interfaces, and the CLI (8.x)
//!   is the first consumer that must.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md

use std::collections::{BTreeMap, BTreeSet};

use tokio::sync::Mutex;
use tracing::{info, instrument, warn};
use zbus::Connection;
use zbus::fdo::ObjectManager;
use zbus::names::InterfaceName;
use zbus::object_server::{Interface, InterfaceRef};
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

use crate::dbus::path;
use crate::error::Error;

/// What is exported, and the only way to change it.
///
/// Not [`Clone`]: the registry owns the object tree, and [`Drop`] is part of how it
/// guarantees the tree goes away with it. Share it as an `Arc<ObjectRegistry>` — every
/// method takes `&self`.
pub struct ObjectRegistry {
    connection: Connection,
    /// Path → the interfaces exported there. Ordered, which is what lets removals run
    /// deepest-first: a child's path is a strict extension of its parent's, so it sorts
    /// after it and reverse order is children-before-parents.
    objects: Mutex<BTreeMap<ObjectPath<'static>, BTreeSet<InterfaceName<'static>>>>,
}

impl ObjectRegistry {
    /// Exports `org.freedesktop.DBus.ObjectManager` at `/org/scanbus` and returns the
    /// registry that owns it.
    ///
    /// Call this **before** requesting the bus name ([`crate::dbus::request_name`]):
    /// the manager has to answer `GetManagedObjects` from the moment the name resolves,
    /// or a client that raced the daemon's startup gets an error where the contract
    /// promises an empty dict.
    ///
    /// # Errors
    ///
    /// [`Error::ObjectServer`] if zbus refused the export.
    pub async fn new(connection: Connection) -> Result<Self, Error> {
        let registry = Self {
            connection,
            objects: Mutex::new(BTreeMap::new()),
        };

        // Through `add`, so that the manager is tracked like everything else and
        // `shutdown` takes it down last.
        registry.add(path::manager(), ObjectManager).await?;

        Ok(registry)
    }

    /// Exports `interface` at `path`, publishing it to every `ObjectManager` client.
    ///
    /// # Errors
    ///
    /// [`Error::AlreadyExported`] if that interface is already at that path — two parts
    /// of the daemon believing they own one object — and [`Error::ObjectServer`] if
    /// zbus refused.
    #[instrument(level = "info", skip_all, fields(path = %path, interface = %I::name()))]
    pub async fn add<I>(&self, path: OwnedObjectPath, interface: I) -> Result<(), Error>
    where
        I: Interface,
    {
        let name = I::name();
        let path = path.into_inner();

        // Held across the export so that the map and the object server cannot be
        // observed disagreeing, and so that two tasks adding the same path serialise.
        let mut objects = self.objects.lock().await;

        let exported = self
            .connection
            .object_server()
            .at(&path, interface)
            .await
            .map_err(|source| Error::ObjectServer {
                operation: "exporting",
                path: path.clone().into(),
                interface: name.clone(),
                source,
            })?;

        if !exported {
            return Err(Error::AlreadyExported {
                path: path.into(),
                interface: name,
            });
        }

        objects.entry(path).or_default().insert(name);
        info!("exported");

        Ok(())
    }

    /// A handle on an interface already exported at `path`.
    ///
    /// The way a property changes after the object was published: mutate through
    /// [`InterfaceRef::get_mut`] and emit `PropertiesChanged` with the returned
    /// [`InterfaceRef`]'s signal emitter — the two together are what `Scanner1`'s
    /// [`update`](crate::dbus::scanner::update) does. It lives here for the same reason
    /// [`ObjectRegistry::add`] does: `object_server()` is called in one place, so what
    /// the registry believes and what the bus serves cannot drift.
    ///
    /// # Errors
    ///
    /// [`Error::NotExported`] when that interface is not at that path — including when
    /// the object went away between a caller looking it up and using it, which a
    /// discovery session racing `StopDiscovery` can genuinely do.
    pub async fn interface<I>(&self, path: &ObjectPath<'_>) -> Result<InterfaceRef<I>, Error>
    where
        I: Interface,
    {
        self.connection
            .object_server()
            .interface::<_, I>(path)
            .await
            .map_err(|source| match source {
                zbus::Error::InterfaceNotFound => Error::NotExported {
                    path: path.clone().into_owned().into(),
                },
                source => Error::ObjectServer {
                    operation: "looking up",
                    path: path.clone().into_owned().into(),
                    interface: I::name(),
                    source,
                },
            })
    }

    /// Unexports every interface at `path`, and nothing below it.
    ///
    /// For a scanner — whose buttons and jobs are children — use
    /// [`ObjectRegistry::remove_subtree`]. This one refuses rather than orphaning them:
    /// see the module documentation on what zbus does to a node's children when its
    /// last interface goes.
    ///
    /// # Errors
    ///
    /// [`Error::NotExported`] if nothing is there, [`Error::HasDescendants`] if
    /// something is exported below it, [`Error::ObjectServer`] if zbus refused an
    /// unexport.
    pub async fn remove_object(&self, path: &ObjectPath<'_>) -> Result<(), Error> {
        let path = path.clone().into_owned();
        let mut objects = self.objects.lock().await;

        if !objects.contains_key(&path) {
            return Err(Error::NotExported { path: path.into() });
        }

        let descendants = Self::descendants(&objects, &path).count();
        if descendants > 0 {
            return Err(Error::HasDescendants {
                path: path.into(),
                descendants,
            });
        }

        let interfaces = objects.remove(&path).unwrap_or_default();

        Self::unexport(&self.connection, &path, &interfaces).await
    }

    /// Unexports `path` and everything under it, children first.
    ///
    /// This is how a scanner goes away: its buttons and jobs are children, and they
    /// have to be unexported before it. Children first for two reasons — zbus would
    /// otherwise drop them along with their parent's node and emit nothing (see the
    /// module documentation), and a client watching a scanner disappear should see its
    /// buttons and jobs go before the scanner itself, not after.
    ///
    /// # Errors
    ///
    /// [`Error::NotExported`] if neither `path` nor any descendant is exported,
    /// [`Error::ObjectServer`] for the first unexport zbus refused.
    pub async fn remove_subtree(&self, path: &ObjectPath<'_>) -> Result<(), Error> {
        let root = path.clone().into_owned();
        let mut objects = self.objects.lock().await;

        let matched: Vec<ObjectPath<'static>> = objects
            .keys()
            .filter(|candidate| **candidate == root)
            .chain(Self::descendants(&objects, &root))
            .cloned()
            .collect();

        if matched.is_empty() {
            return Err(Error::NotExported { path: root.into() });
        }

        // Reverse of the ordering: a child's path is a strict extension of its
        // parent's, so it sorts after it.
        for path in matched.into_iter().rev() {
            let interfaces = objects.remove(&path).unwrap_or_default();
            Self::unexport(&self.connection, &path, &interfaces).await?;
        }

        Ok(())
    }

    /// Takes the whole tree down, deepest object first.
    ///
    /// Idempotent, and infallible on purpose: this runs on the shutdown path, where a
    /// failed unexport is worth a log line and nothing else — the process is about to
    /// drop the connection, which unexports everything anyway.
    pub async fn shutdown(&self) {
        let mut objects = self.objects.lock().await;
        let tree = std::mem::take(&mut *objects);
        let count = tree.len();

        for (path, interfaces) in tree.into_iter().rev() {
            if let Err(error) = Self::unexport(&self.connection, &path, &interfaces).await {
                warn!(%path, %error, "could not unexport on shutdown");
            }
        }

        if count > 0 {
            info!(objects = count, "object tree removed");
        }
    }

    /// The interfaces exported at `path`, or `None` if the object is not there.
    pub async fn interfaces(
        &self,
        path: &ObjectPath<'_>,
    ) -> Option<BTreeSet<InterfaceName<'static>>> {
        let path = path.clone().into_owned();
        self.objects.lock().await.get(&path).cloned()
    }

    /// Every exported path, in tree order.
    ///
    /// The registry's view, which is the one to compare against `GetManagedObjects`
    /// when they are suspected of having drifted.
    pub async fn paths(&self) -> Vec<ObjectPath<'static>> {
        self.objects.lock().await.keys().cloned().collect()
    }

    /// How many objects are exported, the manager included.
    pub async fn len(&self) -> usize {
        self.objects.lock().await.len()
    }

    /// Whether nothing is exported at all — never true while the registry is alive,
    /// since the manager itself is one of its objects.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// The exported paths strictly below `path`.
    ///
    /// The test is on `path` plus a separator, not on `path` alone: `…/scanner/a` and
    /// `…/scanner/ab` are siblings, and a bare prefix match would take the second down
    /// with the first.
    fn descendants<'objects>(
        objects: &'objects BTreeMap<ObjectPath<'static>, BTreeSet<InterfaceName<'static>>>,
        path: &ObjectPath<'_>,
    ) -> impl Iterator<Item = &'objects ObjectPath<'static>> {
        let prefix = format!("{path}/");
        objects
            .keys()
            .filter(move |candidate| candidate.as_str().starts_with(&prefix))
    }

    #[instrument(level = "info", skip_all, fields(path = %path, interfaces = interfaces.len()))]
    async fn unexport(
        connection: &Connection,
        path: &ObjectPath<'static>,
        interfaces: &BTreeSet<InterfaceName<'static>>,
    ) -> Result<(), Error> {
        for interface in interfaces {
            connection
                .object_server()
                .remove_named(path, interface.clone())
                .await
                .map_err(|source| Error::ObjectServer {
                    operation: "unexporting",
                    path: path.clone().into(),
                    interface: interface.clone(),
                    source,
                })?;
        }

        info!("unexported");

        Ok(())
    }
}

impl Drop for ObjectRegistry {
    /// Last-resort unexport for a registry dropped without [`ObjectRegistry::shutdown`].
    ///
    /// Dropping the registry has to leave the bus clean, but unexporting is `async` and
    /// `drop` is not, so the removals are handed to the runtime. That is best effort by
    /// construction — outside a runtime, or during its shutdown, the task never runs —
    /// which is why `shutdown().await` is the supported path and this only warns.
    ///
    /// A daemon *exiting* needs neither: the connection goes with the process and the
    /// bus forgets every object it served. What this covers is a registry replaced
    /// while the connection lives on.
    fn drop(&mut self) {
        let tree = std::mem::take(self.objects.get_mut());
        if tree.is_empty() {
            return;
        }

        warn!(
            objects = tree.len(),
            "object registry dropped with objects still exported; \
             shutdown() should have run first"
        );

        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            warn!("no runtime to unexport them on; they go with the connection");
            return;
        };

        let connection = self.connection.clone();
        handle.spawn(async move {
            for (path, interfaces) in tree.into_iter().rev() {
                if let Err(error) = Self::unexport(&connection, &path, &interfaces).await {
                    warn!(%path, %error, "could not unexport on drop");
                }
            }
        });
    }
}
