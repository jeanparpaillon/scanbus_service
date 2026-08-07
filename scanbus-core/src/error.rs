//! Error taxonomy for the core crate.
//!
//! These are plain Rust errors. Mapping them onto `zbus::fdo::Error` is the daemon's
//! job — doing it here would put D-Bus into the dependency tree of the one crate that
//! must not have it.

use thiserror::Error;

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
#[derive(Debug, Error)]
pub enum BackendError {
    /// The backend does not know this scanner, or it disappeared between calls.
    #[error("unknown scanner: {0}")]
    UnknownScanner(String),

    /// The operation is not implemented by this backend.
    #[error("{operation} is not supported by backend {backend}")]
    Unsupported {
        /// Backend identifier, as returned by `ScannerBackend::id`.
        backend: &'static str,
        /// The operation that was attempted.
        operation: &'static str,
    },

    /// Anything the backend could not classify further.
    #[error("{0}")]
    Other(String),
}
