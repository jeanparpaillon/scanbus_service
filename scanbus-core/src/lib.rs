//! Core types and traits shared by the scanbus daemon and its backends.
//!
//! This crate has no D-Bus dependency, on purpose. The domain model ([`model`]), the
//! backend seam ([`backend`]) and the error taxonomy ([`error`]) are all testable
//! without a bus connection and without a physical scanner, neither of which exists
//! in CI.

pub mod backend;
pub mod error;
pub mod model;
pub mod pairing;
pub mod path;
pub mod profile;

pub use backend::{ButtonPressedEvent, PairingProgress, RestoreDisposition, ScannerBackend};
pub use error::{BackendError, InvalidScannerId, ParseError};
pub use model::{
    ButtonInfo, ButtonsCapability, Capabilities, ColorMode, JobState, PageFormat, PairingState,
    ProfileKind, RawPage, ScannerId, ScannerInfo, Source, Status, Value,
};
pub use pairing::{
    PairOutcome, PairingMachine, PairingStore, PairingStoreError, Restorable, UnpairError,
};
pub use profile::{ProfileProcessor, ProfileResult};
