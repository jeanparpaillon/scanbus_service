//! Pairing persistence stores.
//!
//! `PairingStore` is the seam between the pairing state machine and durable state.
//! This module provides:
//! - `MemoryPairingStore` for tests and ephemeral runs
//! - `JsonPairingStore` for issue 4.1 persistence (`$XDG_CONFIG_HOME/scanbus/pairings.json`)

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use scanbus_core::{
    ButtonInfo, PairingStore, PairingStoreError, ProfileKind, Restorable, ScannerId, ScannerInfo,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

const STORE_VERSION: u32 = 1;

/// The pairings this daemon has made, for as long as this process lives.
#[derive(Default)]
pub struct MemoryPairingStore {
    paired: Mutex<BTreeMap<ScannerId, ScannerInfo>>,
}

impl MemoryPairingStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// The scanners recorded as paired, in id order.
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
        info!(id = %scanner.id, "pairing recorded (memory store)");
        Ok(())
    }

    async fn forget(&self, scanner_id: &ScannerId) -> Result<(), PairingStoreError> {
        if self.lock().remove(scanner_id).is_some() {
            info!(id = %scanner_id, "pairing forgotten");
        }
        Ok(())
    }

    async fn restorable(&self) -> Result<Vec<Restorable>, PairingStoreError> {
        Ok(self
            .lock()
            .values()
            .cloned()
            .map(|scanner| Restorable {
                scanner,
                // No `Connected` tracking in the ephemeral store: tests that need it use
                // `JsonPairingStore`.
                connected: false,
            })
            .collect())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedScanner {
    scanner: ScannerInfo,
    default_profile: Option<ProfileKind>,
    buttons: BTreeMap<u32, ButtonInfo>,
    /// Whether a listener was running for this scanner — the restore path (4.2) reads
    /// this to decide whether to call `start_listening` again. Defaulted for stores
    /// written before this field existed: a scanner such a file names was not
    /// necessarily connected, and "not connected" is the safe reading.
    #[serde(default)]
    connected: bool,
}

impl PersistedScanner {
    fn new(scanner: ScannerInfo) -> Self {
        Self {
            scanner,
            default_profile: None,
            buttons: BTreeMap::new(),
            connected: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedStore {
    version: u32,
    scanners: BTreeMap<ScannerId, PersistedScanner>,
}

impl Default for PersistedStore {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            scanners: BTreeMap::new(),
        }
    }
}

/// Versioned JSON store for pairings and scanner-side assignments.
pub struct JsonPairingStore {
    path: PathBuf,
    state: Mutex<PersistedStore>,
}

impl JsonPairingStore {
    /// Real daemon store in `$XDG_CONFIG_HOME/scanbus/pairings.json`.
    pub fn new() -> Self {
        Self::with_path(default_store_path())
    }

    /// Store at `path`.
    pub fn with_path(path: PathBuf) -> Self {
        let state = load_or_reset(&path);
        Self {
            path,
            state: Mutex::new(state),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PersistedStore> {
        self.state.lock().expect("pairing store lock poisoned")
    }

    fn flush_locked(&self, state: &PersistedStore) -> Result<(), PairingStoreError> {
        persist(&self.path, state).map_err(PairingStoreError::new)
    }
}

impl Default for JsonPairingStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PairingStore for JsonPairingStore {
    async fn save_paired(&self, scanner: &ScannerInfo) -> Result<(), PairingStoreError> {
        let mut state = self.lock();
        state
            .scanners
            .entry(scanner.id.clone())
            .and_modify(|entry| entry.scanner = scanner.clone())
            .or_insert_with(|| PersistedScanner::new(scanner.clone()));
        self.flush_locked(&state)?;
        info!(id = %scanner.id, store = %self.path.display(), "pairing persisted");
        Ok(())
    }

    async fn forget(&self, scanner_id: &ScannerId) -> Result<(), PairingStoreError> {
        let mut state = self.lock();
        if state.scanners.remove(scanner_id).is_some() {
            self.flush_locked(&state)?;
            info!(id = %scanner_id, store = %self.path.display(), "pairing forgotten");
        }
        Ok(())
    }

    async fn save_default_profile(
        &self,
        scanner_id: &ScannerId,
        profile: Option<ProfileKind>,
    ) -> Result<(), PairingStoreError> {
        let mut state = self.lock();
        if let Some(scanner) = state.scanners.get_mut(scanner_id) {
            scanner.default_profile = profile;
            self.flush_locked(&state)?;
        } else {
            debug!(id = %scanner_id, "skipping default profile persistence for unpaired scanner");
        }
        Ok(())
    }

    async fn save_button(
        &self,
        scanner_id: &ScannerId,
        button: &ButtonInfo,
    ) -> Result<(), PairingStoreError> {
        let mut state = self.lock();
        if let Some(scanner) = state.scanners.get_mut(scanner_id) {
            scanner.buttons.insert(button.index, button.clone());
            self.flush_locked(&state)?;
        } else {
            debug!(id = %scanner_id, index = button.index, "skipping button persistence for unpaired scanner");
        }
        Ok(())
    }

    async fn save_connected(
        &self,
        scanner_id: &ScannerId,
        connected: bool,
    ) -> Result<(), PairingStoreError> {
        let mut state = self.lock();
        if let Some(scanner) = state.scanners.get_mut(scanner_id) {
            if scanner.connected != connected {
                scanner.connected = connected;
                self.flush_locked(&state)?;
            }
        } else {
            debug!(id = %scanner_id, "skipping Connected persistence for unpaired scanner");
        }
        Ok(())
    }

    async fn restorable(&self) -> Result<Vec<Restorable>, PairingStoreError> {
        Ok(self
            .lock()
            .scanners
            .values()
            .map(|entry| Restorable {
                scanner: entry.scanner.clone(),
                connected: entry.connected,
            })
            .collect())
    }
}

fn default_store_path() -> PathBuf {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(config_home)
            .join("scanbus")
            .join("pairings.json");
    }

    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("scanbus")
        .join("pairings.json")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn load_or_reset(path: &Path) -> PersistedStore {
    match load(path) {
        Ok(state) => state,
        Err(reason) => {
            rename_aside(path, &reason);
            PersistedStore::default()
        }
    }
}

fn load(path: &Path) -> Result<PersistedStore, String> {
    if !path.exists() {
        return Ok(PersistedStore::default());
    }

    let bytes = fs::read(path)
        .map_err(|error| format!("cannot read store {}: {error}", path.display()))?;
    let loaded: PersistedStore = serde_json::from_slice(&bytes)
        .map_err(|error| format!("cannot parse store {}: {error}", path.display()))?;

    if loaded.version > STORE_VERSION {
        return Err(format!(
            "store version {} is newer than supported {}",
            loaded.version, STORE_VERSION
        ));
    }
    if loaded.version != STORE_VERSION {
        return Err(format!(
            "store version {} is unsupported (expected {})",
            loaded.version, STORE_VERSION
        ));
    }

    Ok(loaded)
}

fn rename_aside(path: &Path, reason: &str) {
    if !path.exists() {
        return;
    }

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let aside = path.with_extension(format!("json.unreadable.{stamp}"));

    match fs::rename(path, &aside) {
        Ok(()) => warn!(
            from = %path.display(),
            to = %aside.display(),
            reason,
            "pairing store is unreadable or unsupported; renamed aside and starting empty"
        ),
        Err(error) => warn!(
            path = %path.display(),
            %error,
            reason,
            "pairing store is unreadable or unsupported; starting empty without renaming"
        ),
    }
}

fn persist(path: &Path, state: &PersistedStore) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("cannot resolve parent directory for {}", path.display()))?;
    ensure_config_dir(parent)?;

    let payload = serde_json::to_vec_pretty(state)
        .map_err(|error| format!("cannot serialize pairing store: {error}"))?;

    let tmp = path.with_extension("json.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp)
        .map_err(|error| format!("cannot open temporary store {}: {error}", tmp.display()))?;

    file.write_all(&payload)
        .map_err(|error| format!("cannot write temporary store {}: {error}", tmp.display()))?;
    file.sync_all()
        .map_err(|error| format!("cannot fsync temporary store {}: {error}", tmp.display()))?;

    fs::rename(&tmp, path)
        .map_err(|error| format!("cannot replace store {}: {error}", path.display()))?;

    let dir = File::open(parent)
        .map_err(|error| format!("cannot open store directory {}: {error}", parent.display()))?;
    dir.sync_all().map_err(|error| {
        format!(
            "cannot fsync store directory {} after rename: {error}",
            parent.display()
        )
    })?;

    Ok(())
}

fn ensure_config_dir(path: &Path) -> Result<(), String> {
    if !path.exists() {
        fs::create_dir_all(path).map_err(|error| {
            format!(
                "cannot create pairing store directory {}: {error}",
                path.display()
            )
        })?;
    }

    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        format!(
            "cannot enforce mode 0700 on pairing store directory {}: {error}",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use scanbus_core::{Capabilities, Status};
    use tempfile::TempDir;

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
    async fn a_pairing_is_recorded_and_removed_in_memory() {
        let store = MemoryPairingStore::new();
        assert!(store.paired().is_empty());

        store.save_paired(&scanner()).await.unwrap();
        assert!(store.contains(&scanner().id));
        assert_eq!(store.paired(), vec![scanner()]);

        store.forget(&scanner().id).await.unwrap();
        assert!(store.paired().is_empty());
    }

    #[tokio::test]
    async fn json_store_round_trips_pairing_and_assignments() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("scanbus").join("pairings.json");
        let store = JsonPairingStore::with_path(path.clone());

        let mut button = ButtonInfo::new(2, "Scan to OCR", false);
        button.profile = Some(ProfileKind::Document);

        store.save_paired(&scanner()).await.unwrap();
        store
            .save_default_profile(&scanner().id, Some(ProfileKind::Image))
            .await
            .unwrap();
        store.save_button(&scanner().id, &button).await.unwrap();

        let loaded = load(&path).unwrap();
        let entry = loaded.scanners.get(&scanner().id).unwrap();
        assert_eq!(entry.scanner, scanner());
        assert_eq!(entry.default_profile, Some(ProfileKind::Image));
        assert_eq!(entry.buttons.get(&2).unwrap().profile, Some(ProfileKind::Document));
    }

    #[tokio::test]
    async fn a_future_version_file_is_renamed_aside_and_treated_as_empty() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("pairings.json");
        fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": STORE_VERSION + 1,
                "scanners": {}
            }))
            .unwrap(),
        )
        .unwrap();

        let store = JsonPairingStore::with_path(path.clone());
        store.save_paired(&scanner()).await.unwrap();

        let entries: Vec<PathBuf> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert!(entries.iter().any(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("pairings.json.unreadable."))
        }));
        assert!(entries.iter().any(|p| p.file_name().and_then(|n| n.to_str()) == Some("pairings.json")));
    }
}
