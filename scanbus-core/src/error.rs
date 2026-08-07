//! Error taxonomy for the core crate.
//!
//! These are plain Rust errors. Mapping them onto `zbus::fdo::Error` is the daemon's
//! job — doing it here would put D-Bus into the dependency tree of the one crate that
//! must not have it.

use thiserror::Error;

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
