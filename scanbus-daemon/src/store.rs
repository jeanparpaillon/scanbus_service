//! [`MemoryPairingStore`]: where a pairing is durable, until [`4.1`] gives it a file.
//!
//! [`PairingStore`] is the seam 1.4 put between the state machine and persistence, and
//! 2.3 is the first issue that has to hand the machine a concrete one. It is deliberately
//! **not** a stub that answers `Ok(())` and forgets: the machine's contract is that
//! `save_paired` happens-before `Done` and `forget` happens-before `Paired=false`, and a
//! store that records nothing cannot show that either rule holds. This one records, so
//! the daemon's own tests can assert on it, and [`4.1`] replaces it with the same
//! behaviour backed by a file.
//!
//! What it does not do is survive a restart. That is the whole of 4.1, and it is why the
//! daemon logs a line at startup rather than leaving a user to work out why their paired
//! scanner is gone after a `systemctl --user restart`.
//!
//! [`4.1`]: https://github.com/jeanparpaillon/scanbus_service/issues/16

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use scanbus_core::{PairingStore, PairingStoreError, ScannerId, ScannerInfo};
use tracing::info;

/// The pairings this daemon has made, for as long as this process lives.
#[derive(Default)]
pub struct MemoryPairingStore {
    /// Keyed by [`ScannerInfo::id`], the same key the object tree uses. A `std` mutex
    /// because nothing here awaits while holding it.
    paired: Mutex<BTreeMap<ScannerId, ScannerInfo>>,
}

impl MemoryPairingStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// The scanners recorded as paired, in id order.
    ///
    /// What [`4.2`] will read at startup, and what a test asserts `Unpair()` removed.
    ///
    /// [`4.2`]: https://github.com/jeanparpaillon/scanbus_service/issues/17
    pub fn paired(&self) -> Vec<ScannerInfo> {
        self.lock().values().cloned().collect()
    }

    /// Whether this scanner is recorded as paired.
    pub fn contains(&self, scanner_id: &ScannerId) -> bool {
        self.lock().contains_key(scanner_id)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<ScannerId, ScannerInfo>> {
        self.paired.lock().expect("pairing store lock poisoned")
    }
}

#[async_trait]
impl PairingStore for MemoryPairingStore {
    async fn save_paired(&self, scanner: &ScannerInfo) -> Result<(), PairingStoreError> {
        self.lock().insert(scanner.id.clone(), scanner.clone());
        info!(id = %scanner.id, "pairing recorded (in memory only until 4.1)");
        Ok(())
    }

    async fn forget(&self, scanner_id: &ScannerId) -> Result<(), PairingStoreError> {
        // Idempotent per the trait: a scanner that was never in here is not an error.
        if self.lock().remove(scanner_id).is_some() {
            info!(id = %scanner_id, "pairing forgotten");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use scanbus_core::{Capabilities, Status};

    use super::*;

    fn scanner() -> ScannerInfo {
        ScannerInfo {
            id: ScannerId::from_backend("mock", "usb:001:002").unwrap(),
            name: "Brother MFC-L2710DW".to_owned(),
            backend: "proprietary:brother".to_owned(),
            address: "usb:001:002".to_owned(),
            capabilities: Capabilities::default(),
            status: Status::Online,
        }
    }

    #[tokio::test]
    async fn a_pairing_is_recorded_and_removed() {
        let store = MemoryPairingStore::new();
        assert!(store.paired().is_empty());

        store.save_paired(&scanner()).await.unwrap();
        assert!(store.contains(&scanner().id));
        assert_eq!(store.paired(), vec![scanner()]);

        store.forget(&scanner().id).await.unwrap();
        assert!(store.paired().is_empty());
    }

    /// The trait's rule, and what makes a client's retried `Unpair()` succeed twice.
    #[tokio::test]
    async fn forgetting_an_unknown_scanner_is_not_an_error() {
        let store = MemoryPairingStore::new();
        store.forget(&scanner().id).await.unwrap();
        store.forget(&scanner().id).await.unwrap();
    }
}
