//! HP backend: HPLIP walk-up scanning, with `hpssd` as the source of button events.
//!
//! Skeleton only. Discovery and the `hpssd` D-Bus subscription land in workstream 6;
//! the `ScannerBackend` impl lands with the trait in issue 1.3.
//!
//! The crate is compiled only when the daemon is built with its `hplip` feature,
//! because everything it will do shells out to hardware-specific tooling that no CI
//! runner has.

/// Backend identifier, as it will be reported by `ScannerBackend::id`.
pub const ID: &str = "hplip";
