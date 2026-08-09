//! The D-Bus surface: the connection, the name, the object tree.
//!
//! Startup happens in three steps, and the order is the contract:
//!
//! 1. [`connect`] — a session-bus connection that owns no name yet.
//! 2. [`ObjectRegistry::new`] and whatever else has to be exported.
//! 3. [`request_name`] — `org.scanbus` appears, and the daemon is reachable.
//!
//! Requesting the name last is what makes the name mean "this service is ready".
//! A client that resolves `org.scanbus` and immediately calls `GetManagedObjects` must
//! not race the export of the manager, and 4.2 needs the stronger version of the same
//! property: paired scanners restored from disk are exported before the name appears,
//! so no client ever sees the daemon with an empty tree it will fill in a moment.
//!
//! Session bus rather than system bus, per implementation plan §7: this is per-user and
//! per-graphical-session, which matches how `brscan-skey` already runs.

pub mod button;
pub mod convert;
pub mod error;
pub mod manager;
pub mod objects;
pub mod path;
pub mod scanner;

use tracing::{info, instrument};
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::{Connection, connection::Builder};

pub use button::Button1;
pub use error::ScanbusError;
pub use manager::Manager1;
pub use objects::ObjectRegistry;
pub use scanner::Scanner1;

/// The well-known name this daemon owns on the session bus.
pub const BUS_NAME: &str = "org.scanbus";

/// Opens a session-bus connection, without requesting any name.
///
/// # Errors
///
/// [`Error::Connect`](crate::Error::Connect) when there is no session bus to talk to —
/// fatal, since a per-session service has nothing to fall back to.
#[instrument(level = "info", skip_all)]
pub async fn connect() -> Result<Connection, crate::Error> {
    let connection = Builder::session()
        .map_err(crate::Error::Connect)?
        .build()
        .await
        .map_err(crate::Error::Connect)?;

    info!(
        unique_name = %connection.unique_name().map_or("<none>", |name| name.as_str()),
        "connected to the session bus"
    );

    Ok(connection)
}

/// Requests [`BUS_NAME`], refusing to be queued for it.
///
/// `DoNotQueue` and nothing else. The two flags zbus would otherwise add by default —
/// `AllowReplacement` and `ReplaceExisting` — are both wrong here:
///
/// - `AllowReplacement` would let a second instance take the name away from a daemon
///   that is mid-scan, leaving the first owning nothing while it still holds the
///   backend's listener.
/// - Without `DoNotQueue`, a second instance sits in the queue as a process that is
///   running, answers nothing, and owns no name — the failure this issue exists to
///   avoid, because it looks like a working daemon to everything except a client.
///
/// So a second instance loses immediately and audibly. `request_name_with_flags` is
/// what makes that explicit: [`Connection::request_name`] would apply
/// `enumflags2::BitFlags::default()`, which for [`RequestNameFlags`] is all three.
///
/// # Errors
///
/// [`Error::NameTaken`](crate::Error::NameTaken) when another process already owns the
/// name; [`Error::NameRequest`](crate::Error::NameRequest) if the call itself failed.
#[instrument(level = "info", skip_all, fields(name = BUS_NAME))]
pub async fn request_name(connection: &Connection) -> Result<(), crate::Error> {
    let reply = connection
        // `.into()` builds the `BitFlags` without naming `enumflags2`, which is zbus's
        // dependency and not ours: pinning a version of it here to spell one constant
        // would turn a bump on their side into a type error on ours.
        .request_name_with_flags(BUS_NAME, RequestNameFlags::DoNotQueue.into())
        .await
        .map_err(|source| match source {
            zbus::Error::NameTaken => crate::Error::NameTaken { name: BUS_NAME },
            source => crate::Error::NameRequest {
                name: BUS_NAME,
                source,
            },
        })?;

    match reply {
        RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => {
            info!("owning the bus name");
            Ok(())
        }
        // `Exists` already came back as `zbus::Error::NameTaken` above, and `InQueue`
        // cannot happen with `DoNotQueue` — but neither means we are serving.
        reply => Err(crate::Error::NameNotOwned {
            name: BUS_NAME,
            reply,
        }),
    }
}
