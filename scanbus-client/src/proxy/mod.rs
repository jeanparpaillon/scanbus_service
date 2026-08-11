//! The five interfaces of [`scanbus-dbus-api.md`], as `zbus` proxies.
//!
//! Written once, here, because two consumers need them: the CLI and the daemon's own
//! conformance suite ([2.8]). Written twice they diverge — the copy in the test suite
//! gets fixed when an interface changes, and the CLI's is found to be stale by a user —
//! whereas one crate turns an interface change into a compile error in both places at the
//! same time ([`scanbus-cli.md`] §2).
//!
//! # These are the wire, not the model
//!
//! Every getter here returns what §3–§6 say the property carries: `Status` is a `String`,
//! `Capabilities` a `HashMap<String, OwnedValue>`. That is deliberate. A proxy that
//! parsed on the way through would have no way to hand back a `Status` the daemon sent
//! and this version does not know, and `monitor` (§3) exists precisely to show that. The
//! typed reading is one layer up, in [`crate::scanner::ScannerState`], which is fallible
//! and says which property it choked on.
//!
//! # What is not here yet
//!
//! `Button1`, `Job1` and `Profile1` started as specification-first proxies. `Profile1` is
//! now implemented server-side (3.1), while `Job1` remains transient and only exists for
//! in-flight scans.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md
//! [2.5]: https://github.com/jeanparpaillon/scanbus_service/issues/9
//! [2.6]: https://github.com/jeanparpaillon/scanbus_service/issues/10
//! [2.8]: https://github.com/jeanparpaillon/scanbus_service/issues/12
//! [3.1]: https://github.com/jeanparpaillon/scanbus_service/issues/13

mod button;
mod job;
mod manager;
mod profile;
mod scanner;

pub use button::Button1Proxy;
pub use job::Job1Proxy;
pub use manager::Manager1Proxy;
pub use profile::Profile1Proxy;
pub use scanner::Scanner1Proxy;

use zbus::Connection;
use zbus::fdo::{ObjectManagerProxy, PropertiesProxy};
use zbus::zvariant::ObjectPath;

use crate::connect::BUS_NAME;
use crate::error::{Error, Result};

/// The `org.freedesktop.DBus.ObjectManager` on `/org/scanbus`.
///
/// §2 is explicit that there is no `ScannerFound` signal because this is one: every
/// scanner, button and job appearing or disappearing reaches a client that subscribed
/// here. Built through this helper rather than by hand so that the destination and the
/// path are spelled in one place — the two things a hand-built proxy silently gets wrong,
/// since an `ObjectManagerProxy` with a default path would watch `/` and see nothing.
///
/// # Errors
///
/// [`Error::Bus`] if the proxy cannot be built, which at this point means only that the
/// connection is gone.
pub async fn object_manager(connection: &Connection) -> Result<ObjectManagerProxy<'static>> {
    ObjectManagerProxy::builder(connection)
        .destination(BUS_NAME)
        .map_err(Error::Bus)?
        .path(scanbus_core::path::ROOT)
        .map_err(Error::Bus)?
        .build()
        .await
        .map_err(Error::Bus)
}

/// The `org.freedesktop.DBus.Properties` interface of one object.
///
/// The three things a client does with it — `GetAll` for a snapshot, `Set` for a write,
/// `PropertiesChanged` for the stream — are all reached from here; [`crate::watch`] is
/// what sequences them without a race.
///
/// # Errors
///
/// [`Error::Bus`] if the path is not one the bus accepts.
pub async fn properties<'a, P>(connection: &Connection, path: P) -> Result<PropertiesProxy<'static>>
where
    P: TryInto<ObjectPath<'a>>,
    P::Error: Into<zbus::Error>,
{
    // The bound zbus's own builders use: a `&str` may fail to parse, an
    // `OwnedObjectPath` cannot, and both should be accepted without the caller
    // converting first.
    let path = path
        .try_into()
        .map_err(|error| Error::Bus(error.into()))?
        .to_owned();

    PropertiesProxy::builder(connection)
        .destination(BUS_NAME)
        .map_err(Error::Bus)?
        .path(path)
        .map_err(Error::Bus)?
        .build()
        .await
        .map_err(Error::Bus)
}
