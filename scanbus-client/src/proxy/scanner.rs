//! `org.scanbus.Scanner1` — the properties and methods of [`scanbus-dbus-api.md`] §3.
//!
//! No `default_path`: there is one of these per scanner, and the path is what identifies
//! it. Build one with [`Scanner1Proxy::for_scanner`], which is the only place the path
//! layout of §1 has to be known.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md

use std::collections::HashMap;

use scanbus_core::{ScannerId, path};
use zbus::proxy;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

/// One scanner, paired or merely discovered (§1).
#[proxy(interface = "org.scanbus.Scanner1", default_service = "org.scanbus")]
pub trait Scanner1 {
    /// Stable identifier — also the `{id}` element of this object's path.
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;

    /// Human-readable name, as the device reports it.
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;

    /// Which subsystem found it: `"sane"`, `"escl"`, `"proprietary:brother"`, …
    ///
    /// Free-form, and the way a client tells which backend won a deduplication (§9).
    #[zbus(property)]
    fn backend(&self) -> zbus::Result<String>;

    /// Connection URI or device path.
    #[zbus(property)]
    fn address(&self) -> zbus::Result<String>;

    /// What the device can do, as the `a{sv}` of §3.
    ///
    /// [`crate::convert::capabilities`] reads it into the typed model.
    #[zbus(property)]
    fn capabilities(&self) -> zbus::Result<HashMap<String, OwnedValue>>;

    /// The profiles this daemon will run for this scanner.
    #[zbus(property)]
    fn supported_profiles(&self) -> zbus::Result<Vec<String>>;

    /// Whether this scanner is paired with this host.
    ///
    /// Independent of [`status`](Scanner1Proxy::status): a paired scanner that is
    /// switched off is `Paired=true, Status="offline"` (§9).
    #[zbus(property)]
    fn paired(&self) -> zbus::Result<bool>;

    /// Whether the host is listening for this scanner's events.
    #[zbus(property)]
    fn connected(&self) -> zbus::Result<bool>;

    /// Reachability: `"offline"`, `"online"`, `"busy"`, `"error"`.
    #[zbus(property)]
    fn status(&self) -> zbus::Result<String>;

    /// The profile applied to data that arrives without one, `""` for none.
    #[zbus(property)]
    fn default_profile(&self) -> zbus::Result<String>;

    /// Sets the fallback profile; `""` clears it.
    ///
    /// # Errors
    ///
    /// `org.freedesktop.DBus.Error.InvalidArgs` for anything outside
    /// `SupportedProfiles`. Not `org.scanbus.Error.UnsupportedProfile`, although that is
    /// §8's name for the condition: a property `Set` is served by zbus's own
    /// `org.freedesktop.DBus.Properties` on the daemon side and can only fail with a
    /// standard name. A caller matching on [`crate::ScanbusError`] therefore sees
    /// [`crate::ScanbusError::Other`] here, and the message names the profiles that
    /// would have been accepted.
    #[zbus(property)]
    fn set_default_profile(&self, value: &str) -> zbus::Result<()>;

    /// Where the pairing process is — `"none"`, `"pairing"`, `"installing_backend"`,
    /// `"done"`, `"failed"` (§6).
    #[zbus(property)]
    fn pairing_state(&self) -> zbus::Result<String>;

    /// Failure detail while `PairingState="failed"`, empty otherwise.
    #[zbus(property)]
    fn pairing_error(&self) -> zbus::Result<String>;

    /// Starts pairing and returns immediately (§9).
    ///
    /// The reply says only that the process has started; the verdict arrives as
    /// `PropertiesChanged`. [`crate::watch::ScannerWatch`] is the raceless way to wait
    /// for it — subscribing after this call loses the transition on a scanner whose
    /// backend is already installed.
    ///
    /// A second `Pair()` while one is in progress succeeds and changes nothing.
    ///
    /// # Errors
    ///
    /// `org.scanbus.Error.AlreadyPaired` when `Paired=true`.
    fn pair(&self, options: HashMap<String, OwnedValue>) -> zbus::Result<()>;

    /// Cancels a pairing in progress, resetting `PairingState` to `"none"`.
    ///
    /// Succeeds when there is nothing to cancel: a client that lost the race with a
    /// transition should not have to tell that apart from a real failure.
    fn cancel_pairing(&self) -> zbus::Result<()>;

    /// Removes the association, and the object with it unless discovery still sees it.
    ///
    /// # Errors
    ///
    /// `org.scanbus.Error.NotPaired` when there is no association to remove.
    fn unpair(&self) -> zbus::Result<()>;

    /// Declares the host ready to receive.
    ///
    /// `options` may carry `{"profile": "document"}` to set the session profile.
    ///
    /// # Errors
    ///
    /// `org.scanbus.Error.NotReachable` when `Status="offline"`,
    /// `org.scanbus.Error.NotPaired`, `org.scanbus.Error.UnsupportedProfile`.
    fn connect(&self, options: HashMap<String, OwnedValue>) -> zbus::Result<()>;

    /// Stops listening on the host side.
    fn disconnect(&self) -> zbus::Result<()>;

    /// Host-driven scan, returning the `Job1` path it created.
    ///
    /// **Optional in §3**, which for a contract read by several clients means "absent in
    /// practice on some daemons" — a call against one that omits it comes back as
    /// `org.freedesktop.DBus.Error.UnknownMethod`, i.e. [`crate::ScanbusError::Other`],
    /// and that is the shape a caller has to be ready for ([`scanbus-cli.md`] §11).
    ///
    /// # Errors
    ///
    /// `org.scanbus.Error.NotConnected`, `org.scanbus.Error.Busy`, or the unknown-method
    /// error above.
    ///
    /// [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md
    fn scan(&self, options: HashMap<String, OwnedValue>) -> zbus::Result<OwnedObjectPath>;
}

impl<'a> Scanner1Proxy<'a> {
    /// A proxy for the scanner with this id, at the path §1 gives it.
    ///
    /// # Errors
    ///
    /// [`Error::Bus`](crate::Error::Bus) if the proxy cannot be built.
    pub async fn for_scanner(
        connection: &zbus::Connection,
        id: &ScannerId,
    ) -> crate::Result<Scanner1Proxy<'a>> {
        // Infallible by construction: a `ScannerId` is exactly the D-Bus path charset,
        // and `path::scanner` composes it with literals — see `scanbus_core::path`.
        let path = zbus::zvariant::ObjectPath::try_from(path::scanner(id))
            .expect("a path built from a ScannerId is always valid");

        Self::builder(connection)
            .path(path)
            .map_err(crate::Error::Bus)?
            .build()
            .await
            .map_err(crate::Error::Bus)
    }

    /// The scanner's id, read from the path rather than from the `Id` property.
    ///
    /// One fewer round trip, and it cannot disagree with the object it is a proxy for:
    /// §1 makes the id the path element, so the two are the same fact.
    pub fn scanner_id(&self) -> Option<ScannerId> {
        path::scanner_id(self.inner().path().as_str())
    }
}
