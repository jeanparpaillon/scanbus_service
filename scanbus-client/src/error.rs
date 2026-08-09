//! The named errors of [`scanbus-dbus-api.md`] §8, decoded exactly once.
//!
//! §8 fixes seven error names under `org.scanbus.Error`, and they arrive here as a
//! [`zbus::Error::MethodError`] carrying the name as a string. Every caller that compares
//! that string with `==` is a place where a typo compiles and then silently classifies a
//! refusal as an unknown failure — the CLI would print the right message and exit 1
//! instead of 7 ([`scanbus-cli.md`] §8), and a test asserting on the wrong spelling would
//! pass for the wrong reason. So the decode happens here, once, and both consumers match
//! on a variant.
//!
//! # Why `Other` carries two fields where the issue says one
//!
//! An unknown name has to survive: a daemon newer than this client answering
//! `org.scanbus.Error.Xyz` must not become "some error". It also has to survive
//! *printably* — the CLI's error line is `scanbus: <what failed>: <name>: <message>` for
//! every error, named or not, so an `Other` that kept only the name would make the
//! unknown case the one where the daemon's explanation is dropped. Both fields, then,
//! and [`ScanbusError::name`] answers uniformly across the variants.
//!
//! # Two types, not one
//!
//! [`ScanbusError`] is a *reply*: the daemon was reached, understood the call and refused
//! it. [`Error`] is everything a client call can end as, that one included — no bus, no
//! reply, a value that does not decode. Folding them together would mean either giving
//! `ScanbusError` variants that have no D-Bus name, or giving the transport failures one
//! they do not have.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

use std::fmt;

use crate::connect::BUS_NAME;
use crate::convert::DecodeError;

/// A refusal from the daemon, named the way §8 names it.
///
/// Every variant carries the human-readable detail the error reply's body held, which is
/// what the daemon put there — `"scanner mock_usb is already paired"` and the like.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::enum_variant_names,
    reason = "the variant names are the wire names of §8, not ours to shorten"
)]
pub enum ScanbusError {
    /// `org.scanbus.Error.NotReachable` — the device did not answer.
    NotReachable(String),
    /// `org.scanbus.Error.AlreadyPaired` — `Pair()` on a scanner already paired.
    AlreadyPaired(String),
    /// `org.scanbus.Error.NotPaired` — an operation that needs a pairing, without one.
    NotPaired(String),
    /// `org.scanbus.Error.NotConnected` — the host is not listening.
    NotConnected(String),
    /// `org.scanbus.Error.BackendInstallFailed` — a system dependency did not install.
    BackendInstallFailed(String),
    /// `org.scanbus.Error.UnsupportedProfile` — a profile this daemon will not run.
    UnsupportedProfile(String),
    /// `org.scanbus.Error.Busy` — the device is busy.
    Busy(String),
    /// Any other named error: the standard `org.freedesktop.DBus.Error.*` family, and
    /// anything a daemon newer than this client has invented.
    ///
    /// Kept verbatim rather than collapsed, so `scanbus` can print the name it was given
    /// and a bug report says which name arrived.
    Other {
        /// The error name as it came off the bus.
        name: String,
        /// The reply body, empty when the daemon sent none.
        message: String,
    },
}

impl ScanbusError {
    /// The prefix §8 puts every name of its own under.
    pub const PREFIX: &'static str = "org.scanbus.Error.";

    /// Decodes an error name and its detail into a variant.
    ///
    /// The seven names of §8 and nothing else: `org.freedesktop.DBus.Error.InvalidArgs`
    /// is a real, named error and lands in [`ScanbusError::Other`] with its name intact,
    /// because §8 gives it no exit code of its own and pretending otherwise would invent
    /// one.
    pub fn from_reply(name: &str, message: &str) -> Self {
        let detail = message.to_owned();

        match name.strip_prefix(Self::PREFIX) {
            Some("NotReachable") => Self::NotReachable(detail),
            Some("AlreadyPaired") => Self::AlreadyPaired(detail),
            Some("NotPaired") => Self::NotPaired(detail),
            Some("NotConnected") => Self::NotConnected(detail),
            Some("BackendInstallFailed") => Self::BackendInstallFailed(detail),
            Some("UnsupportedProfile") => Self::UnsupportedProfile(detail),
            Some("Busy") => Self::Busy(detail),
            _ => Self::Other {
                name: name.to_owned(),
                message: detail,
            },
        }
    }

    /// The error name, as it travelled on the bus.
    pub fn name(&self) -> &str {
        match self {
            Self::NotReachable(_) => "org.scanbus.Error.NotReachable",
            Self::AlreadyPaired(_) => "org.scanbus.Error.AlreadyPaired",
            Self::NotPaired(_) => "org.scanbus.Error.NotPaired",
            Self::NotConnected(_) => "org.scanbus.Error.NotConnected",
            Self::BackendInstallFailed(_) => "org.scanbus.Error.BackendInstallFailed",
            Self::UnsupportedProfile(_) => "org.scanbus.Error.UnsupportedProfile",
            Self::Busy(_) => "org.scanbus.Error.Busy",
            Self::Other { name, .. } => name,
        }
    }

    /// The detail the daemon sent, empty when it sent none.
    pub fn message(&self) -> &str {
        match self {
            Self::NotReachable(detail)
            | Self::AlreadyPaired(detail)
            | Self::NotPaired(detail)
            | Self::NotConnected(detail)
            | Self::BackendInstallFailed(detail)
            | Self::UnsupportedProfile(detail)
            | Self::Busy(detail)
            | Self::Other {
                message: detail, ..
            } => detail,
        }
    }

    /// The exit code `scanbus` ends with for this error ([`scanbus-cli.md`] §8).
    ///
    /// The table lives beside the enum rather than in the CLI because it is the half of
    /// a contract whose other half is the daemon's ([2.7]): a name the daemon can emit
    /// and this maps to 1 is a bug in one of the two, and keeping the mapping here is
    /// what lets one test enumerate both sides. `Other` is exit 1 by construction —
    /// "unclassified failure" is what that code means, and §8 hands out no others.
    ///
    /// [2.7]: https://github.com/jeanparpaillon/scanbus_service/issues/11
    /// [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::NotReachable(_) => 5,
            Self::AlreadyPaired(_) => 6,
            Self::NotPaired(_) => 7,
            Self::NotConnected(_) => 8,
            Self::BackendInstallFailed(_) => 9,
            Self::UnsupportedProfile(_) => 10,
            Self::Busy(_) => 11,
            Self::Other { .. } => 1,
        }
    }
}

impl fmt::Display for ScanbusError {
    /// `<name>: <message>`, or just the name when the daemon sent no body.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.message() {
            "" => f.write_str(self.name()),
            message => write!(f, "{}: {message}", self.name()),
        }
    }
}

impl std::error::Error for ScanbusError {}

/// Anything a call through this crate can end as.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The daemon refused the call, with a name §8 defines or one it does not.
    #[error(transparent)]
    Call(#[from] ScanbusError),

    /// Everything under the method call: no connection, no reply, a malformed message.
    ///
    /// Never a refusal — [`From<zbus::Error>`](Error::from) peels those off into
    /// [`Error::Call`] first, so matching on this arm cannot accidentally catch one.
    #[error("D-Bus: {0}")]
    Bus(zbus::Error),

    /// A value the daemon sent that this version cannot make sense of.
    #[error(transparent)]
    Decode(#[from] DecodeError),

    /// A property the daemon announced as invalidated rather than changed.
    ///
    /// This daemon emits `PropertiesChanged` with values for every property of §3, so a
    /// watcher can follow a scanner from the signals alone. An invalidation means that
    /// stopped being true, and the honest answer is to say so rather than to keep
    /// reporting a value that is now a guess: the caller re-reads with
    /// `Properties.GetAll` and starts again.
    #[error("{property} was invalidated rather than changed; re-read it with GetAll")]
    Invalidated {
        /// The property the daemon invalidated.
        property: String,
    },

    /// `--no-activate`, and the name has no owner.
    ///
    /// Not folded into [`Error::Bus`]: "the daemon is not running and I was told not to
    /// start it" is a deliberate outcome with its own exit code (3), not a failure of
    /// the bus.
    #[error("{BUS_NAME} has no owner and activation was disabled")]
    NotRunning,
}

impl From<zbus::Error> for Error {
    /// Peels a named refusal out of whatever zbus reports.
    ///
    /// Two shapes reach a client and both are refusals: zbus decodes the standard names
    /// into [`zbus::fdo::Error`] and leaves everything else — every `org.scanbus.Error.*`
    /// — as a [`zbus::Error::MethodError`]. Handling only the second is the bug this
    /// conversion exists to prevent: a client that matched `MethodError` alone would see
    /// an `InvalidArgs` as a transport failure.
    fn from(error: zbus::Error) -> Self {
        match error {
            zbus::Error::MethodError(name, detail, _) => Self::Call(ScanbusError::from_reply(
                name.as_str(),
                detail.as_deref().unwrap_or_default(),
            )),
            zbus::Error::FDO(error) => {
                let name = zbus::DBusError::name(error.as_ref()).as_str().to_owned();
                let message = zbus::DBusError::description(error.as_ref())
                    .unwrap_or_default()
                    .to_owned();
                Self::Call(ScanbusError::Other { name, message })
            }
            other => Self::Bus(other),
        }
    }
}

impl From<zbus::fdo::Error> for Error {
    /// The same peel, for the proxies zbus ships.
    ///
    /// `ObjectManagerProxy` and `PropertiesProxy` are typed on [`zbus::fdo::Error`]
    /// rather than [`zbus::Error`], and that type carries both kinds of failure in one
    /// enum: [`zbus::fdo::Error::ZBus`] is "the call did not happen", every other variant
    /// is a named refusal that did. Splitting them here is what keeps
    /// `GetAll` against a job that has just been removed —
    /// `org.freedesktop.DBus.Error.UnknownObject` — classified as a refusal rather than
    /// as a broken connection.
    fn from(error: zbus::fdo::Error) -> Self {
        match error {
            zbus::fdo::Error::ZBus(error) => Self::from(error),
            named => {
                let name = zbus::DBusError::name(&named).as_str().to_owned();
                let message = zbus::DBusError::description(&named)
                    .unwrap_or_default()
                    .to_owned();
                Self::Call(ScanbusError::Other { name, message })
            }
        }
    }
}

/// The result of a call through this crate.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    /// The seven names of §8, and the exit code each is worth.
    #[test]
    fn every_documented_name_decodes_to_its_variant() {
        let cases = [
            ("NotReachable", ScanbusError::NotReachable("x".into()), 5),
            ("AlreadyPaired", ScanbusError::AlreadyPaired("x".into()), 6),
            ("NotPaired", ScanbusError::NotPaired("x".into()), 7),
            ("NotConnected", ScanbusError::NotConnected("x".into()), 8),
            (
                "BackendInstallFailed",
                ScanbusError::BackendInstallFailed("x".into()),
                9,
            ),
            (
                "UnsupportedProfile",
                ScanbusError::UnsupportedProfile("x".into()),
                10,
            ),
            ("Busy", ScanbusError::Busy("x".into()), 11),
        ];

        for (suffix, expected, code) in cases {
            let name = format!("{}{suffix}", ScanbusError::PREFIX);
            let decoded = ScanbusError::from_reply(&name, "x");

            assert_eq!(decoded, expected, "{name}");
            assert_eq!(decoded.name(), name, "the name must survive the decode");
            assert_eq!(decoded.message(), "x");
            assert_eq!(decoded.exit_code(), code, "{name}");
        }
    }

    /// One exit code per name, or a script cannot branch on *why* (§8).
    #[test]
    fn the_documented_names_have_distinct_exit_codes() {
        let mut codes: Vec<u8> = [
            ScanbusError::NotReachable(String::new()),
            ScanbusError::AlreadyPaired(String::new()),
            ScanbusError::NotPaired(String::new()),
            ScanbusError::NotConnected(String::new()),
            ScanbusError::BackendInstallFailed(String::new()),
            ScanbusError::UnsupportedProfile(String::new()),
            ScanbusError::Busy(String::new()),
        ]
        .iter()
        .map(ScanbusError::exit_code)
        .collect();

        let count = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), count, "two named errors share an exit code");
        assert_eq!(codes, [5, 6, 7, 8, 9, 10, 11]);
    }

    /// A daemon newer than this client keeps its name and its explanation.
    #[test]
    fn an_unknown_name_is_kept_verbatim() {
        let decoded = ScanbusError::from_reply("org.scanbus.Error.Xyz", "the lid is open");

        assert_eq!(
            decoded,
            ScanbusError::Other {
                name: "org.scanbus.Error.Xyz".to_owned(),
                message: "the lid is open".to_owned(),
            }
        );
        assert_eq!(decoded.name(), "org.scanbus.Error.Xyz");
        assert_eq!(decoded.exit_code(), 1);
        assert_eq!(
            decoded.to_string(),
            "org.scanbus.Error.Xyz: the lid is open"
        );
    }

    /// The standard family is named too, and must not be mistaken for one of §8's just
    /// because the suffix matches.
    #[test]
    fn a_standard_name_is_other_even_when_its_suffix_collides() {
        for name in [
            "org.freedesktop.DBus.Error.InvalidArgs",
            "org.freedesktop.DBus.Error.UnknownMethod",
            "com.example.Error.Busy",
        ] {
            let decoded = ScanbusError::from_reply(name, "");
            assert!(
                matches!(&decoded, ScanbusError::Other { name: kept, .. } if kept == name),
                "{name} decoded as {decoded:?}"
            );
            assert_eq!(decoded.to_string(), name, "no body, so no trailing colon");
        }
    }

    /// A prefix match is not a name match: `…Error.NotPairedYet` is not `NotPaired`.
    #[test]
    fn a_name_that_merely_starts_like_one_of_ours_is_not_it() {
        assert!(matches!(
            ScanbusError::from_reply("org.scanbus.Error.NotPairedYet", ""),
            ScanbusError::Other { .. }
        ));
        assert!(matches!(
            ScanbusError::from_reply("org.scanbus.Error.Busy.Extra", ""),
            ScanbusError::Other { .. }
        ));
    }

    /// The standard family arrives pre-decoded by zbus and must not look like transport.
    ///
    /// Its sibling — an `org.scanbus.Error.*` arriving as [`zbus::Error::MethodError`] —
    /// cannot be built here, because that variant holds a whole [`zbus::message::Message`]
    /// that only a real bus produces. `scanbus-daemon/tests/client.rs` asserts it against
    /// a daemon that actually refuses a call.
    #[test]
    fn a_standard_refusal_becomes_a_call_error() {
        let fdo = zbus::Error::FDO(Box::new(zbus::fdo::Error::InvalidArgs(
            "unknown Pair option \"pin\"".to_owned(),
        )));
        match Error::from(fdo) {
            Error::Call(ScanbusError::Other { name, message }) => {
                assert_eq!(name, "org.freedesktop.DBus.Error.InvalidArgs");
                assert_eq!(message, "unknown Pair option \"pin\"");
            }
            other => panic!("an fdo error is a refusal, not {other:?}"),
        }
    }

    /// Everything that is not a reply stays transport, so a caller can tell "the daemon
    /// said no" from "the daemon never answered".
    #[test]
    fn a_transport_failure_is_not_a_refusal() {
        assert!(matches!(
            Error::from(zbus::Error::InvalidReply),
            Error::Bus(zbus::Error::InvalidReply)
        ));
    }
}
