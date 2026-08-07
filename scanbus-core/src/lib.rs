//! Core types and traits shared by the scanbus daemon and its backends.
//!
//! This crate has no D-Bus dependency, on purpose. The domain model ([`model`]), the
//! backend seam ([`backend`]) and the error taxonomy ([`error`]) are all testable
//! without a bus connection and without a physical scanner, neither of which exists
//! in CI.

pub mod backend;
pub mod error;
pub mod model;

pub use error::BackendError;
