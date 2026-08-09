//! Getting onto the right bus, and deciding whether the daemon may be started.
//!
//! `scanbus` is a session service ([7.1]), so the session bus is the default and nothing
//! else needs a flag to work. The other two spellings exist for reasons that are not
//! symmetry: `--bus ADDRESS` is what a test harness points at its private
//! `dbus-daemon`, and `--bus system` is there because a user who has installed the
//! daemon system-wide should get a clear failure from the bus rather than from us.
//!
//! # `--no-activate` is a question about the *name*, not about the call
//!
//! The daemon is D-Bus-activated, which means a plain method call against `org.scanbus`
//! starts it. A `--no-activate` implemented as "make the call and see" therefore does the
//! exact thing it was asked not to do, and finds out afterwards. The only implementation
//! that holds is to ask the bus whether the name has an owner *before* the first call —
//! `NameHasOwner` is a call to the bus daemon itself and activates nothing — and to
//! refuse when it does not. That is [`presence`], and it is also what `scanbus status`
//! is: a health check with no side effects ([`scanbus-cli.md`] §3).
//!
//! [7.1]: https://github.com/jeanparpaillon/scanbus_service/issues/26
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

use std::fmt;
use std::str::FromStr;

use zbus::names::BusName;
use zbus::{Connection, fdo::DBusProxy};

use crate::error::{Error, Result};

/// The well-known name the daemon owns.
pub const BUS_NAME: &str = "org.scanbus";

/// Which bus to talk to — the `--bus` option of [`scanbus-cli.md`] §3.
///
/// [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Bus {
    /// The session bus. Where the daemon runs.
    #[default]
    Session,
    /// The system bus.
    System,
    /// A raw address, e.g. `unix:path=/tmp/dbus-XYZ`.
    Address(String),
}

impl Bus {
    /// Opens a connection, owning no name.
    ///
    /// # Errors
    ///
    /// [`Error::Bus`] when there is no such bus, or the address does not parse.
    pub async fn connect(&self) -> Result<Connection> {
        let builder = match self {
            Self::Session => zbus::connection::Builder::session(),
            Self::System => zbus::connection::Builder::system(),
            Self::Address(address) => zbus::connection::Builder::address(address.as_str()),
        }
        .map_err(Error::Bus)?;

        builder.build().await.map_err(Error::Bus)
    }
}

impl FromStr for Bus {
    type Err = std::convert::Infallible;

    /// `session`, `system`, or an address.
    ///
    /// Infallible on purpose: an address this crate cannot parse is the bus library's
    /// verdict, not ours, and giving it here would mean two places that decide what a
    /// D-Bus address is. A typo like `--bus sesion` therefore fails at [`Bus::connect`]
    /// with the address parser's own message.
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s {
            "session" => Self::Session,
            "system" => Self::System,
            address => Self::Address(address.to_owned()),
        })
    }
}

impl fmt::Display for Bus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session => f.write_str("session"),
            Self::System => f.write_str("system"),
            Self::Address(address) => f.write_str(address),
        }
    }
}

/// Whether the daemon is there, could be started, or is not installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// The name has an owner: the daemon is running now.
    Running,
    /// No owner, but the bus knows how to start one.
    Activatable,
    /// No owner and no activation file — nothing would start.
    Absent,
}

impl Presence {
    /// Whether a call would reach a daemon, starting one if need be.
    pub const fn is_reachable(self) -> bool {
        matches!(self, Self::Running | Self::Activatable)
    }
}

impl fmt::Display for Presence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Running => "running",
            Self::Activatable => "activatable",
            Self::Absent => "absent",
        })
    }
}

/// Asks the bus about [`BUS_NAME`], without starting anything.
///
/// Both questions go to `org.freedesktop.DBus`, which is the bus daemon itself and is
/// always there — neither `NameHasOwner` nor `ListActivatableNames` can trigger
/// activation of *our* name, which is the whole point.
///
/// # Errors
///
/// [`Error::Bus`] if the bus daemon itself cannot be reached — through
/// [`From<zbus::fdo::Error>`](Error::from), which is what turns the bus's own refusals
/// into an [`Error::Call`] rather than hiding them here.
pub async fn presence(connection: &Connection) -> Result<Presence> {
    let bus = DBusProxy::new(connection).await?;
    let name = BusName::try_from(BUS_NAME).expect("BUS_NAME is a well-known name");

    if bus.name_has_owner(name).await? {
        return Ok(Presence::Running);
    }

    let activatable = bus.list_activatable_names().await?;
    Ok(
        if activatable.iter().any(|name| name.as_str() == BUS_NAME) {
            Presence::Activatable
        } else {
            Presence::Absent
        },
    )
}

/// The unique name currently owning [`BUS_NAME`], or `None` when nobody does.
///
/// The companion to [`presence`] for a health check that has to name the process it
/// found: `:1.42` is what a developer needs to go from "the daemon is running" to
/// `busctl --user status :1.42`, and it is the one fact that distinguishes the daemon
/// that answered from the one that was restarted since. Like [`presence`], this is a
/// call to the bus daemon and activates nothing.
///
/// # Errors
///
/// [`Error::Call`] if the bus refuses for any reason other than the name being
/// unowned — which is the `None` here, not a failure.
pub async fn owner(connection: &Connection) -> Result<Option<String>> {
    let bus = DBusProxy::new(connection).await?;
    let name = BusName::try_from(BUS_NAME).expect("BUS_NAME is a well-known name");

    match bus.get_name_owner(name).await {
        Ok(owner) => Ok(Some(owner.as_str().to_owned())),
        // The bus's way of saying "nobody", and the reason this returns an `Option`: a
        // caller that had to match on an error name to read an ordinary answer would be
        // one more place where a typo compiles.
        Err(zbus::fdo::Error::NameHasNoOwner(_)) => Ok(None),
        Err(error) => Err(Error::from(error)),
    }
}

/// Connects, and — when `activate` is false — refuses to go on if that would start the
/// daemon.
///
/// `activate: true` returns as soon as the connection is up: the first method call is
/// what activates, and letting it is the default. `activate: false` costs one round trip
/// to the bus daemon and answers [`Error::NotRunning`] unless the name already has an
/// owner. Note that `Activatable` is *not* good enough in that case — activatable means
/// the next call would start it.
///
/// # Errors
///
/// [`Error::Bus`] when the bus cannot be reached, [`Error::NotRunning`] when
/// `activate` is false and the name has no owner.
pub async fn connect(bus: &Bus, activate: bool) -> Result<Connection> {
    let connection = bus.connect().await?;

    if !activate && presence(&connection).await? != Presence::Running {
        return Err(Error::NotRunning);
    }

    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_names_are_names_and_everything_else_is_an_address() {
        assert_eq!("session".parse::<Bus>().unwrap(), Bus::Session);
        assert_eq!("system".parse::<Bus>().unwrap(), Bus::System);
        assert_eq!(
            "unix:path=/tmp/dbus-XYZ".parse::<Bus>().unwrap(),
            Bus::Address("unix:path=/tmp/dbus-XYZ".to_owned())
        );
        assert_eq!(Bus::default(), Bus::Session);
    }

    /// The round trip a CLI needs to echo back what it was given.
    #[test]
    fn a_bus_prints_the_way_it_was_written() {
        for spelling in ["session", "system", "unix:path=/tmp/dbus-XYZ"] {
            assert_eq!(spelling.parse::<Bus>().unwrap().to_string(), spelling);
        }
    }

    /// A typo is an address, and fails when it is used rather than when it is parsed —
    /// with the address parser's message, which names what it could not read.
    #[tokio::test]
    async fn a_misspelled_bus_name_fails_at_connect_time() {
        let error = "sesion"
            .parse::<Bus>()
            .unwrap()
            .connect()
            .await
            .expect_err("sesion is not an address");
        assert!(matches!(error, Error::Bus(_)), "{error:?}");
    }

    #[test]
    fn only_a_running_or_activatable_daemon_is_reachable() {
        assert!(Presence::Running.is_reachable());
        assert!(Presence::Activatable.is_reachable());
        assert!(!Presence::Absent.is_reachable());

        assert_eq!(Presence::Running.to_string(), "running");
        assert_eq!(Presence::Activatable.to_string(), "activatable");
        assert_eq!(Presence::Absent.to_string(), "absent");
    }
}
