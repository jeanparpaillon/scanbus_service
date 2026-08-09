//! `org.scanbus.Manager1` — the three methods of [`scanbus-dbus-api.md`] §2.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md

use std::collections::HashMap;

use zbus::proxy;
use zbus::zvariant::OwnedValue;

/// The manager at `/org/scanbus`.
///
/// Both defaults are set, so `Manager1Proxy::new(&connection)` is enough: there is
/// exactly one manager, at one path, on one name.
#[proxy(
    interface = "org.scanbus.Manager1",
    default_service = "org.scanbus",
    default_path = "/org/scanbus"
)]
pub trait Manager1 {
    /// Starts discovery through every active backend, or the subset `filters` names.
    ///
    /// `filters` is `{"backends": ["sane","avahi"]}` or empty. Returns as soon as the
    /// session is running — the scanners arrive afterwards, as `InterfacesAdded` on the
    /// `ObjectManager` of the same object.
    ///
    /// # Errors
    ///
    /// `org.freedesktop.DBus.Error.InvalidArgs` for a filter the daemon cannot honour:
    /// an unknown key, or a backend id no backend answers to (§9).
    fn start_discovery(&self, filters: HashMap<String, OwnedValue>) -> zbus::Result<()>;

    /// Stops the discovery in progress; succeeds when none is running.
    ///
    /// Returns once the unpaired scanner objects are off the bus, so the
    /// `InterfacesRemoved` signals precede the reply. Note that discovery carries no
    /// notion of who asked for it until [2.9] — see [`scanbus-cli.md`] §7 for why a
    /// client should think twice before calling this.
    ///
    /// [2.9]: https://github.com/jeanparpaillon/scanbus_service/issues/34
    /// [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md
    fn stop_discovery(&self) -> zbus::Result<()>;

    /// The profile types this daemon will actually run.
    ///
    /// A subset of `["image","document","email","ocr"]`, and currently a proper one: a
    /// profile absent here is one the daemon refuses, so a UI that offers the four from
    /// §6 offers two that fail at scan time.
    fn get_profile_types(&self) -> zbus::Result<Vec<String>>;
}
