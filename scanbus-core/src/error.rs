//! Error taxonomy for the core crate.
//!
//! These are plain Rust errors. Mapping them onto `zbus::fdo::Error` is the daemon's
//! job — doing it here would put D-Bus into the dependency tree of the one crate that
//! must not have it.

use thiserror::Error;

use crate::model::{ProfileKind, ScannerId};

/// A string that cannot be a [`ScannerId`](crate::model::ScannerId).
///
/// Carries the offending value: the id is derived from backend-supplied data, so the
/// interesting part of the failure is always *what* the backend produced.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid scanner id {value:?}: {reason}")]
pub struct InvalidScannerId {
    /// The rejected string.
    pub value: String,
    /// Why it was rejected.
    pub reason: &'static str,
}

/// A string that names no variant of one of the model's closed enumerations.
///
/// The D-Bus API spells `Status`, `PairingState` and the profile kinds as strings; this
/// is what parsing one that is not in the documented set produces.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{value:?} is not a valid {kind}")]
pub struct ParseError {
    /// Name of the type that refused the string, e.g. `"Status"`.
    pub kind: &'static str,
    /// The rejected string.
    pub value: String,
}

impl ParseError {
    /// Builds a parse failure for `kind` from the string that was rejected.
    pub fn new(kind: &'static str, value: impl Into<String>) -> Self {
        Self {
            kind,
            value: value.into(),
        }
    }
}

/// Failure reported by a [`ScannerBackend`](crate::backend::ScannerBackend).
///
/// The variants exist to be mapped one by one onto the named D-Bus errors of
/// [`scanbus-dbus-api.md`] §8; that mapping is the daemon's (2.7), and this is the
/// vocabulary it maps *from*. A backend that reaches for [`BackendError::Other`] is
/// choosing `org.freedesktop.DBus.Error.Failed` for its caller, which is why the
/// variant is last and named after what it is.
///
/// Three of the §8 errors are deliberately absent: `AlreadyPaired`, `NotPaired` and
/// `NotConnected` are statements about daemon state — the registry knows whether
/// `Pair()` has run, and a backend asked to do something in the wrong order is a bug on
/// our side, not a device condition.
///
/// `Clone`, for two reasons that both come up immediately: a test double has to hand
/// the same configured failure to every call
/// ([`MockHandle::fail_ensure_installed`](crate::backend::mock::MockHandle::fail_ensure_installed)),
/// and the pairing task copies the message into
/// [`PairingState::Failed`](crate::model::PairingState::Failed) while still returning
/// the error.
///
/// [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BackendError {
    /// The backend does not know this scanner, or it disappeared between calls.
    ///
    /// → `org.freedesktop.DBus.Error.UnknownObject`.
    #[error("unknown scanner: {0}")]
    UnknownScanner(ScannerId),

    /// No such job on that scanner — including a job whose pages have already been
    /// fetched, since [`fetch_pages`](crate::backend::ScannerBackend::fetch_pages) may
    /// be called only once per job id.
    ///
    /// → `org.freedesktop.DBus.Error.UnknownObject`.
    #[error("scanner {scanner} has no job {job:?}")]
    UnknownJob {
        /// The scanner the job was looked for on.
        scanner: ScannerId,
        /// The job id, as the daemon minted it.
        job: String,
    },

    /// The device did not answer: powered off, off the network, unplugged.
    ///
    /// → `org.scanbus.Error.NotReachable`.
    #[error("scanner {scanner} is not reachable: {detail}")]
    NotReachable {
        /// The scanner that did not answer.
        scanner: ScannerId,
        /// What was tried, in a form worth putting in front of a user.
        detail: String,
    },

    /// The device is doing something else — a scan is running, or another host holds it.
    ///
    /// → `org.scanbus.Error.Busy`.
    #[error("scanner {0} is busy")]
    Busy(ScannerId),

    /// A system dependency could not be installed.
    ///
    /// → `org.scanbus.Error.BackendInstallFailed`. The only error
    /// [`ensure_installed`](crate::backend::ScannerBackend::ensure_installed) is
    /// expected to produce, and the one the pairing state machine turns into
    /// `PairingState="failed"`.
    #[error("installing {package} failed: {detail}")]
    InstallFailed {
        /// Package, `.deb` or step that failed, e.g. `"brscan5"`.
        package: String,
        /// Why — an exit status, an HTTP status, a checksum mismatch.
        detail: String,
    },

    /// The backend cannot map this profile onto anything the device offers.
    ///
    /// → `org.scanbus.Error.UnsupportedProfile`. Note this is the *device's* refusal:
    /// profiles this daemon has no processor for ([`ProfileKind::is_supported`]) are
    /// refused by the daemon before a backend is ever asked.
    #[error("profile {0} is not supported by this backend")]
    UnsupportedProfile(ProfileKind),

    /// The operation is not implemented by this backend.
    ///
    /// → `org.freedesktop.DBus.Error.NotSupported`.
    #[error("{operation} is not supported by backend {backend}")]
    Unsupported {
        /// Backend identifier, as returned by `ScannerBackend::id`.
        backend: &'static str,
        /// The operation that was attempted.
        operation: &'static str,
    },

    /// Anything the backend could not classify further.
    ///
    /// → `org.freedesktop.DBus.Error.Failed`.
    #[error("{0}")]
    Other(String),
}
