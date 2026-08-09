//! A client for the scanbus D-Bus API: the proxies, the named errors, the property
//! watching — written once and consumed twice.
//!
//! The CLI takes this crate as a dependency and `scanbus-daemon` as a dev-dependency, so
//! a change to an interface is a compile error in the binary *and* in the daemon's own
//! conformance tests at the same time, rather than a runtime surprise in whichever of the
//! two was not updated ([`scanbus-cli.md`] §2).
//!
//! ```no_run
//! use futures_util::StreamExt as _;
//! use scanbus_client::{Bus, ScannerWatch, connect, proxy::Scanner1Proxy};
//! use scanbus_core::{PairingState, ScannerId};
//!
//! # async fn example() -> scanbus_client::Result<()> {
//! let connection = connect(&Bus::Session, true).await?;
//! let id = ScannerId::new("mock_usb_3A001_3A002").unwrap();
//! let scanner = Scanner1Proxy::for_scanner(&connection, &id).await?;
//!
//! // Subscribe, call, then read: anything else races the daemon (§7).
//! let watch = ScannerWatch::subscribe(&connection, scanbus_core::path::scanner(&id)).await?;
//! scanner.pair(Default::default()).await?;
//!
//! let mut states = watch.states().await?;
//! while let Some(state) = states.next().await {
//!     match state?.pairing {
//!         PairingState::Done => break,
//!         PairingState::Failed(why) => return Err(scanbus_client::Error::Bus(
//!             zbus::Error::Failure(why),
//!         )),
//!         _ => {}
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # The dependency direction is the design
//!
//! `scanbus-cli` → `scanbus-client` → `scanbus-core`, one way. Nothing here may depend on
//! `scanbus-daemon`, and the pull to do so is real — the daemon has the object-path
//! helpers and knows how to read a capability map. The answer is to move such a helper
//! *down* into `scanbus-core`, which is what happened to [`scanbus_core::path`]; the
//! reverse would make the CLI depend on the service it is meant to be able to test
//! against. `scripts/check-deps.sh` fails the build on either violation.
//!
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

pub mod connect;
pub mod convert;
pub mod error;
pub mod proxy;
pub mod scanner;
pub mod watch;

pub use connect::{BUS_NAME, Bus, Presence, connect, presence};
pub use convert::DecodeError;
pub use error::{Error, Result, ScanbusError};
pub use scanner::ScannerState;
pub use watch::{PropertyChanges, PropertyWatch, ScannerStates, ScannerWatch};
