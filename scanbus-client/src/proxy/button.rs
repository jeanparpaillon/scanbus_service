//! `org.scanbus.Button1` — one entry of the device's physical menu ([`scanbus-dbus-api.md`] §5).
//!
//! The interface separates what the firmware imposes from what the host may assign, and
//! the proxy keeps that visible: `DeviceLabel` and `LabelConfigurable` have no setter
//! here because they have none on the bus.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md

use std::collections::HashMap;

use scanbus_core::{ScannerId, path};
use zbus::proxy;
use zbus::zvariant::OwnedValue;

/// One physical key or touchscreen entry of a scanner.
#[proxy(interface = "org.scanbus.Button1", default_service = "org.scanbus")]
pub trait Button1 {
    /// Position in the physical menu, 0-based.
    #[zbus(property)]
    fn index(&self) -> zbus::Result<u32>;

    /// Label the firmware imposes, e.g. `"Scan to E-mail"`; empty when it exposes none.
    #[zbus(property)]
    fn device_label(&self) -> zbus::Result<String>;

    /// Whether the host can push a custom label to the device.
    ///
    /// `false` on classic Brother devices with fixed keys, where only the key→profile
    /// mapping is configurable — and the reason a UI must not offer a rename that would
    /// silently fail (§9).
    #[zbus(property)]
    fn label_configurable(&self) -> zbus::Result<bool>;

    /// Label actually displayed; mirrors `DeviceLabel` when the host has assigned none.
    #[zbus(property)]
    fn label(&self) -> zbus::Result<String>;

    /// Assigns the displayed label.
    ///
    /// # Errors
    ///
    /// Refused when `LabelConfigurable=false`.
    #[zbus(property)]
    fn set_label(&self, value: &str) -> zbus::Result<()>;

    /// Profile triggered when this key is selected, `""` when unassigned.
    ///
    /// Nothing ties this to `DeviceLabel`: a key the firmware calls "Scan to OCR" may be
    /// assigned `document`, and warning the user about the divergence is a config UI's
    /// job (§5).
    #[zbus(property)]
    fn profile(&self) -> zbus::Result<String>;

    /// Assigns the profile this key triggers; `""` clears it.
    ///
    /// The daemon rewrites the backend's configuration and reloads it as a side effect —
    /// invisible here, which is the point.
    ///
    /// # Errors
    ///
    /// Refused for a profile outside `Scanner1.SupportedProfiles`.
    #[zbus(property)]
    fn set_profile(&self, value: &str) -> zbus::Result<()>;

    /// Options for this profile on this key, e.g. a different output folder per key.
    #[zbus(property)]
    fn profile_options(&self) -> zbus::Result<HashMap<String, OwnedValue>>;

    /// Replaces the whole option map.
    ///
    /// Whole, not merged: `a{sv}` has no way to say "leave the others alone", so a
    /// caller changing one key reads, edits and writes back —
    /// [`crate::convert::from_dict`] and [`crate::convert::to_dict`] are that round trip.
    #[zbus(property)]
    fn set_profile_options(&self, value: HashMap<String, OwnedValue>) -> zbus::Result<()>;
}

impl<'a> Button1Proxy<'a> {
    /// A proxy for button `index` of the scanner `id`, at the path §1 gives it.
    ///
    /// # Errors
    ///
    /// [`Error::Bus`](crate::Error::Bus) if the proxy cannot be built.
    pub async fn for_button(
        connection: &zbus::Connection,
        id: &ScannerId,
        index: u32,
    ) -> crate::Result<Button1Proxy<'a>> {
        let path = zbus::zvariant::ObjectPath::try_from(path::button(id, index))
            .expect("a path built from a ScannerId and an index is always valid");

        Self::builder(connection)
            .path(path)
            .map_err(crate::Error::Bus)?
            .build()
            .await
            .map_err(crate::Error::Bus)
    }
}
