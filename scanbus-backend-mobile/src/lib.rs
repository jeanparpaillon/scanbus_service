//! Mobile backend: backend for mobile protocol, actually implemented by Scanbus Android app.
//!
//! The crate is compiled only when the daemon is built with its `mobile` feature,
//! because everything it will do shells out to hardware-specific tooling that no CI
//! runner has.

/// Backend identifier, as it will be reported by `ScannerBackend::id`.
pub const ID: &str = "mobile";
