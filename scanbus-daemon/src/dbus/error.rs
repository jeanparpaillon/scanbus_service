//! [`ScanbusError`]: the named D-Bus errors of [`scanbus-dbus-api.md`] §8.
//!
//! §8 fixes seven error names under `org.scanbus.Error`, and the CLI turns each into its
//! own exit code ([`scanbus-cli.md`] §8) — so a name is part of the contract in the same
//! way a property is, and a method that answers `org.freedesktop.DBus.Error.Failed`
//! where §8 has a name for the condition costs a script the ability to branch on *why*.
//!
//! [`2.7`] owns the full mapping from [`BackendError`](scanbus_core::BackendError); this
//! type is the vocabulary it will map *onto*, and 2.3 uses the three names its own
//! methods can produce.
//!
//! # Why the prefix is `org`
//!
//! `#[derive(DBusError)]` builds every name as `<prefix>.<variant>` with no way to opt a
//! variant out, so a type that has to answer with both `org.scanbus.Error.AlreadyPaired`
//! and `org.freedesktop.DBus.Error.Failed` has to take the prefix down to the one
//! component the two families share. Every variant then spells the rest of its name
//! explicitly, which is more honest than it looks: these strings are the API.
//!
//! # What a property setter can answer, and what it cannot
//!
//! **A `Set` on `org.freedesktop.DBus.Properties` cannot carry one of these names.** zbus
//! serves that interface itself, its `set` returns [`zbus::fdo::Error`], and the reply's
//! error name comes from that type — a custom [`zbus::DBusError`] returned by a property
//! setter is converted into an `fdo::Error` before the reply is built, losing the name.
//! Writing an unsupported `DefaultProfile` therefore answers
//! `org.freedesktop.DBus.Error.InvalidArgs` and not `org.scanbus.Error.UnsupportedProfile`
//! ([`crate::dbus::scanner`] says so at the setter). Methods have no such limit, which is
//! why `Connect({"profile": …})` (2.4) will be able to answer with the §8 name for the
//! same condition.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md
//! [`2.7`]: https://github.com/jeanparpaillon/scanbus_service/issues/11

use zbus::DBusError;

/// A failure of an `org.scanbus` method, named the way §8 names it.
///
/// Every variant carries the human-readable detail as its first field, which is what
/// D-Bus puts in the error reply's body.
#[derive(Debug, DBusError)]
#[zbus(prefix = "org")]
#[allow(
    clippy::enum_variant_names,
    reason = "the variant names are the wire names of §8, not ours to shorten"
)]
pub enum ScanbusError {
    /// The device did not answer — `org.scanbus.Error.NotReachable`.
    #[zbus(name = "scanbus.Error.NotReachable")]
    NotReachable(String),

    /// `Pair()` on a scanner that is already paired — `org.scanbus.Error.AlreadyPaired`.
    ///
    /// §9's second idempotency rule. Distinct from `Pair()` on a scanner *being* paired,
    /// which is a successful no-op.
    #[zbus(name = "scanbus.Error.AlreadyPaired")]
    AlreadyPaired(String),

    /// An operation that needs a pairing, on a scanner that has none —
    /// `org.scanbus.Error.NotPaired`.
    #[zbus(name = "scanbus.Error.NotPaired")]
    NotPaired(String),

    /// The host is not listening — `org.scanbus.Error.NotConnected`. 2.4's.
    #[zbus(name = "scanbus.Error.NotConnected")]
    NotConnected(String),

    /// A system dependency could not be installed —
    /// `org.scanbus.Error.BackendInstallFailed`.
    ///
    /// Not produced by `Pair()`, which cannot fail after it has returned: an install that
    /// fails lands on `PairingState="failed"` with `PairingError` set (§9). This is for
    /// the calls that *do* install synchronously.
    #[zbus(name = "scanbus.Error.BackendInstallFailed")]
    BackendInstallFailed(String),

    /// The profile is not one this daemon will run — `org.scanbus.Error.UnsupportedProfile`.
    #[zbus(name = "scanbus.Error.UnsupportedProfile")]
    UnsupportedProfile(String),

    /// The device is busy — `org.scanbus.Error.Busy`.
    #[zbus(name = "scanbus.Error.Busy")]
    Busy(String),

    /// Anything §8 has no name for — `org.freedesktop.DBus.Error.Failed`.
    ///
    /// The standard name rather than an eighth `org.scanbus.Error.*`: inventing a name
    /// outside §8 would give the CLI an exit code it does not have, and "unclassified
    /// failure" is exactly what exit code 1 already covers.
    #[zbus(name = "freedesktop.DBus.Error.Failed")]
    Failed(String),

    /// The arguments are not ones this daemon can honour —
    /// `org.freedesktop.DBus.Error.InvalidArgs`.
    #[zbus(name = "freedesktop.DBus.Error.InvalidArgs")]
    InvalidArgs(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The strings are the contract: §8's seven, plus the two standard ones.
    #[test]
    fn every_variant_spells_its_documented_name() {
        let cases = [
            (
                ScanbusError::NotReachable(String::new()),
                "org.scanbus.Error.NotReachable",
            ),
            (
                ScanbusError::AlreadyPaired(String::new()),
                "org.scanbus.Error.AlreadyPaired",
            ),
            (
                ScanbusError::NotPaired(String::new()),
                "org.scanbus.Error.NotPaired",
            ),
            (
                ScanbusError::NotConnected(String::new()),
                "org.scanbus.Error.NotConnected",
            ),
            (
                ScanbusError::BackendInstallFailed(String::new()),
                "org.scanbus.Error.BackendInstallFailed",
            ),
            (
                ScanbusError::UnsupportedProfile(String::new()),
                "org.scanbus.Error.UnsupportedProfile",
            ),
            (ScanbusError::Busy(String::new()), "org.scanbus.Error.Busy"),
            (
                ScanbusError::Failed(String::new()),
                "org.freedesktop.DBus.Error.Failed",
            ),
            (
                ScanbusError::InvalidArgs(String::new()),
                "org.freedesktop.DBus.Error.InvalidArgs",
            ),
        ];

        for (error, name) in cases {
            assert_eq!(zbus::DBusError::name(&error).as_str(), name);
        }
    }

    #[test]
    fn the_detail_travels_as_the_error_description() {
        let error = ScanbusError::AlreadyPaired("scanner mock_usb is already paired".to_owned());
        assert_eq!(
            zbus::DBusError::description(&error),
            Some("scanner mock_usb is already paired")
        );
    }
}
