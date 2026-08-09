//! [`Scanner1`]: one object per scanner, paired or not.
//!
//! §1 of [`scanbus-dbus-api.md`] refuses two representations for one scanner: a device
//! found by discovery is the *same* kind of object as a paired one, with `Paired=false`,
//! so a client calls `Pair()` on the object it just saw appear instead of translating a
//! struct into a path. That is the decision this type implements, and the reason the
//! registry ([`crate::scanners`]) has to remember why each object exists.
//!
//! **Properties only, in this iteration.** `Pair`, `CancelPairing`, `Unpair`,
//! `Connect`, `Disconnect` and the writable `DefaultProfile` are 2.3's and 2.4's; what
//! 2.2 needs is an object whose properties a client can read the moment
//! `InterfacesAdded` names it. The properties that belong to the pairing machine are
//! therefore here but read-only and constant — `PairingState` is `"none"` until 2.3
//! feeds it from [`scanbus_core::PairingMachine`] — rather than absent, so that the
//! object a discovery session publishes already has the shape §3 describes.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md

use scanbus_core::{PairingState, ScannerInfo};
use tracing::debug;
use zbus::object_server::InterfaceRef;

use crate::dbus::convert::{self, Dict};

/// The `org.scanbus.Scanner1` object of §3.
///
/// Holds the backend's half of the state ([`ScannerInfo`]) and the host's half beside
/// it. Both are updated through [`update`], never by a caller reaching in: the object
/// server hands out `&mut` under a lock, and a mutation that skips the
/// `PropertiesChanged` emission is invisible to every client.
pub struct Scanner1 {
    info: ScannerInfo,
    paired: bool,
}

impl Scanner1 {
    /// A scanner object carrying `info`, `Paired` as given.
    pub fn new(info: ScannerInfo, paired: bool) -> Self {
        Self { info, paired }
    }

    /// What the backend last reported.
    pub fn info(&self) -> &ScannerInfo {
        &self.info
    }

    /// Whether this scanner is paired with this host.
    ///
    /// Named apart from the `Paired` property getter below, which the interface macro
    /// owns; both read the same field.
    pub fn is_paired(&self) -> bool {
        self.paired
    }
}

#[zbus::interface(name = "org.scanbus.Scanner1")]
impl Scanner1 {
    /// Stable identifier — also the `{id}` element of this object's path.
    #[zbus(property)]
    fn id(&self) -> String {
        self.info.id.to_string()
    }

    /// Human-readable name, as the device reports it.
    #[zbus(property)]
    fn name(&self) -> String {
        self.info.name.clone()
    }

    /// Which subsystem found it: `"sane"`, `"escl"`, `"proprietary:brother"`, …
    ///
    /// Kept on the object precisely so a client can tell which backend won a
    /// deduplication (§9); see [`crate::scanners`] for the rule that decides it.
    #[zbus(property)]
    fn backend(&self) -> String {
        self.info.backend.clone()
    }

    /// Connection URI or device path.
    #[zbus(property)]
    fn address(&self) -> String {
        self.info.address.clone()
    }

    /// What the device can do, as the `a{sv}` of §3.
    #[zbus(property)]
    fn capabilities(&self) -> Dict {
        convert::capabilities(&self.info.capabilities)
    }

    /// The profiles this daemon will actually run for this scanner.
    #[zbus(property)]
    fn supported_profiles(&self) -> Vec<String> {
        self.info
            .supported_profiles()
            .into_iter()
            .map(|kind| kind.as_str().to_owned())
            .collect()
    }

    /// Whether this scanner is paired with this host.
    ///
    /// Independent of [`Scanner1::status`] (§9): a paired scanner that is switched off
    /// is `Paired=true, Status="offline"`.
    #[zbus(property)]
    fn paired(&self) -> bool {
        self.paired
    }

    /// Whether the host is listening for this scanner's events.
    ///
    /// Always `false` until 2.4, which is what owns the listener task; a property that
    /// lied here would have a client skip the `Connect()` it still has to make.
    #[zbus(property)]
    fn connected(&self) -> bool {
        false
    }

    /// Reachability: `"offline"`, `"online"`, `"busy"`, `"error"`.
    #[zbus(property)]
    fn status(&self) -> String {
        self.info.status.as_str().to_owned()
    }

    /// Where the pairing process is — `"none"` until 2.3 drives it.
    #[zbus(property)]
    fn pairing_state(&self) -> String {
        PairingState::None.as_str().to_owned()
    }

    /// Failure detail for `PairingState="failed"`, empty otherwise.
    #[zbus(property)]
    fn pairing_error(&self) -> String {
        PairingState::None.pairing_error().to_owned()
    }
}

/// Applies a fresh [`ScannerInfo`] to an exported object, emitting `PropertiesChanged`
/// for exactly the properties that moved.
///
/// This is what "a rediscovered scanner updates the existing object rather than adding a
/// second one" means in practice (§1): the identity is [`ScannerInfo::id`], and
/// everything else — a device renamed on its front panel, an address that moved with
/// DHCP, a scanner that came back online — is a property change on the object a client
/// is already watching.
///
/// `Id` is deliberately not in the diff: it is the path element, so a change in it is a
/// different object, and the registry never routes one here.
///
/// # Errors
///
/// Whatever zbus failed to emit. A failure here leaves the in-memory value updated and
/// the client's copy stale, which is why the caller logs it rather than swallowing it.
pub async fn update(iface: &InterfaceRef<Scanner1>, info: &ScannerInfo) -> zbus::Result<()> {
    let mut scanner = iface.get_mut().await;
    let previous = std::mem::replace(&mut scanner.info, info.clone());
    let emitter = iface.signal_emitter();

    if previous.name != info.name {
        scanner.name_changed(emitter).await?;
    }
    if previous.backend != info.backend {
        scanner.backend_changed(emitter).await?;
    }
    if previous.address != info.address {
        scanner.address_changed(emitter).await?;
    }
    if previous.capabilities != info.capabilities {
        scanner.capabilities_changed(emitter).await?;
        // `SupportedProfiles` is derived from the info, so it is checked here rather
        // than compared: the derivation is the same one the getter runs.
        if previous.supported_profiles() != info.supported_profiles() {
            scanner.supported_profiles_changed(emitter).await?;
        }
    }
    if previous.status != info.status {
        debug!(id = %info.id, from = %previous.status, to = %info.status, "status changed");
        scanner.status_changed(emitter).await?;
    }

    Ok(())
}

/// Sets `Paired` and tells every client watching.
///
/// # Errors
///
/// Whatever zbus failed to emit.
pub async fn set_paired(iface: &InterfaceRef<Scanner1>, paired: bool) -> zbus::Result<()> {
    let mut scanner = iface.get_mut().await;
    if scanner.paired == paired {
        return Ok(());
    }

    scanner.paired = paired;
    scanner.paired_changed(iface.signal_emitter()).await
}
