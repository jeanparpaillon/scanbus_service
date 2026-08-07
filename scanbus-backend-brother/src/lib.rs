//! Brother backend: `brscan4`/`brscan5` for scanning, `brscan-skey` for the buttons.
//!
//! Skeleton only. Discovery, `ensure_installed` and the `brscan-skey` supervision land
//! in workstream 5; the `ScannerBackend` impl lands with the trait in issue 1.3.
//!
//! The crate is compiled only when the daemon is built with its `brother` feature,
//! because everything it will do shells out to hardware-specific tooling that no CI
//! runner has.

/// Backend identifier, as it will be reported by `ScannerBackend::id`.
pub const ID: &str = "brother-skey";
