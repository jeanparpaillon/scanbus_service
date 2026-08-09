//! Failures of the daemon itself — connecting, owning the name, exporting objects.
//!
//! Distinct from [`BackendError`](scanbus_core::BackendError), which is what a device
//! or a vendor tool did wrong. These are ours: a bus that is not there, a name someone
//! else owns, an object exported twice. None of them is a `Scanner1` method failing, so
//! none of them is mapped onto the named D-Bus errors of §8 — that mapping is 2.7's,
//! and it maps from `BackendError`.

use thiserror::Error;
use zbus::fdo::RequestNameReply;
use zbus::names::InterfaceName;
use zbus::zvariant::OwnedObjectPath;

/// Anything that stops the daemon from serving, or the registry from staying honest.
#[derive(Debug, Error)]
pub enum Error {
    /// No session bus, or it refused the connection.
    ///
    /// Fatal at startup: this is a per-user, per-session service (implementation plan
    /// §7), so there is nothing to fall back to.
    #[error("cannot connect to the session bus")]
    Connect(#[source] zbus::Error),

    /// Another process already owns the bus name.
    ///
    /// The name is requested with `DoNotQueue`, so this is what a second daemon
    /// instance gets — immediately, rather than sitting in the queue owning nothing.
    #[error("{name} is already owned by another process on the session bus")]
    NameTaken {
        /// The well-known name that was refused.
        name: &'static str,
    },

    /// The bus answered `RequestName` with something other than primary ownership.
    ///
    /// Not reachable with the flags used here — `DoNotQueue` leaves only
    /// `PrimaryOwner`, `AlreadyOwner` and the refusal above — but a reply that is
    /// neither means we are not serving and must not pretend otherwise.
    #[error("requesting {name} returned {reply:?} instead of primary ownership")]
    NameNotOwned {
        /// The well-known name that was requested.
        name: &'static str,
        /// What the bus replied.
        reply: RequestNameReply,
    },

    /// `RequestName` itself failed — a malformed name, or the call did not go through.
    #[error("requesting the name {name} failed")]
    NameRequest {
        /// The well-known name that was requested.
        name: &'static str,
        /// The underlying zbus failure.
        #[source]
        source: zbus::Error,
    },

    /// zbus refused to export or unexport an interface.
    #[error("{operation} {interface} at {path} failed")]
    ObjectServer {
        /// `"exporting"` or `"unexporting"`.
        operation: &'static str,
        /// The object path involved.
        path: OwnedObjectPath,
        /// The interface involved.
        interface: InterfaceName<'static>,
        /// The underlying zbus failure.
        #[source]
        source: zbus::Error,
    },

    /// The interface is already exported at that path.
    ///
    /// A bug in the caller rather than a condition: the registry is the single owner of
    /// the object tree, so a second export of the same interface means two pieces of
    /// daemon state believe they own the same object.
    #[error("{interface} is already exported at {path}")]
    AlreadyExported {
        /// The object path involved.
        path: OwnedObjectPath,
        /// The interface involved.
        interface: InterfaceName<'static>,
    },

    /// Nothing is exported at that path.
    #[error("no object is exported at {path}")]
    NotExported {
        /// The object path that was asked for.
        path: OwnedObjectPath,
    },

    /// The object still has exported descendants.
    ///
    /// Refused rather than performed, because zbus destroys a node the moment its last
    /// interface goes — **children and all**, with no `InterfacesRemoved` for them. The
    /// registry would go on tracking objects the bus no longer serves, which is the
    /// drift this whole module exists to prevent. Take the subtree down instead.
    #[error("{path} still has {descendants} exported descendants; remove the subtree instead")]
    HasDescendants {
        /// The object path that was asked for.
        path: OwnedObjectPath,
        /// How many objects are exported below it.
        descendants: usize,
    },

    /// The signal handlers could not be installed.
    ///
    /// Fatal on purpose: without them systemd's `SIGTERM` reaches the default
    /// disposition and the daemon dies without unexporting anything.
    #[error("cannot install signal handlers")]
    Signal(#[source] std::io::Error),
}
