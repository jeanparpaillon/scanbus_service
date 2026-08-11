//! The scanbus daemon, as a library.
//!
//! The binary in `main.rs` is only wiring: it starts a runtime, connects, hands the
//! object tree to [`dbus::ObjectRegistry`] and waits for a signal. Everything it does
//! lives here so that it is reachable from `tests/`, where the D-Bus contract is
//! exercised against a real `dbus-daemon` rather than asserted about in prose.
//!
//! The split from `scanbus-core` runs the other way round: core holds the domain model
//! and the backend seam and must never see zbus (`scripts/check-deps.sh` enforces
//! it), while everything in this crate is allowed to — and this is the only crate that
//! renders the model onto the bus.

pub mod backends;
pub mod dbus;
pub mod discovery;
pub mod error;
pub mod jobs;
pub mod listeners;
pub mod profiles;
pub mod scanners;
pub mod store;

pub use backends::Backends;
pub use discovery::Discovery;
pub use error::Error;
pub use jobs::{JobRegistry, JobTrigger};
pub use listeners::{ButtonEvent, ButtonEventSink, RestartPolicy};
pub use profiles::ProfileRegistry;
pub use scanners::ScannerRegistry;
pub use store::{JsonPairingStore, MemoryPairingStore};
