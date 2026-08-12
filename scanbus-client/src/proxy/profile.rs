//! `org.scanbus.Profile1` — the configurable options of a post-processing profile
//! ([`scanbus-dbus-api.md`] §6).
//!
//! # Open options, and the property that describes them
//!
//! The options themselves stay an `a{sv}`: they are per-profile and open, which is what
//! that signature is for, and the proxy hands them back as the wire carries them. What is
//! no longer open is *which* keys a profile takes — `OptionsSchema` publishes that, the
//! daemon generates it from the same table it validates writes against (10.13), and §6
//! makes it normative over the prose. So the proxy grows the property rather than a typed
//! getter per profile; the typing is one layer up in [`crate::profile::OptionsSchema`],
//! for the same reason [`crate::scanner::ScannerState`] is not in the `Scanner1` proxy.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md
//! [3.1]: https://github.com/jeanparpaillon/scanbus_service/issues/13

use std::collections::HashMap;

use scanbus_core::{ProfileKind, path};
use zbus::proxy;
use zbus::zvariant::OwnedValue;

/// One post-processing profile: `image`, `document`, `email` or `ocr`.
#[proxy(interface = "org.scanbus.Profile1", default_service = "org.scanbus")]
pub trait Profile1 {
    /// The profile's name — the `{name}` element of its path.
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;

    /// The profile's configuration, as the `a{sv}` of §6.
    #[zbus(property)]
    fn options(&self) -> zbus::Result<HashMap<String, OwnedValue>>;

    /// Replaces the configuration.
    ///
    /// Whole, not merged, for the same reason as `Button1.ProfileOptions`: read, edit,
    /// write back.
    ///
    /// # Errors
    ///
    /// `org.freedesktop.DBus.Error.InvalidArgs` for an option this profile does not
    /// have, or a value it cannot use.
    #[zbus(property)]
    fn set_options(&self, value: HashMap<String, OwnedValue>) -> zbus::Result<()>;

    /// What this profile accepts: one `a{sv}` entry per option key, as §6 fixes it.
    ///
    /// Read-only, but not constant — the effective default of `output_folder` changes
    /// with the user's XDG directories — so a client that caches it refreshes on
    /// `PropertiesChanged` rather than reading it once. [`crate::profile::OptionsSchema`]
    /// is what turns it into something to build widgets from.
    #[zbus(property)]
    fn options_schema(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
}

impl<'a> Profile1Proxy<'a> {
    /// A proxy for one profile, at the path §1 gives it.
    ///
    /// # Errors
    ///
    /// [`Error::Bus`](crate::Error::Bus) if the proxy cannot be built. A profile the
    /// daemon does not run has no object, and the failure comes at the first call.
    pub async fn for_profile(
        connection: &zbus::Connection,
        kind: ProfileKind,
    ) -> crate::Result<Profile1Proxy<'a>> {
        let path = zbus::zvariant::ObjectPath::try_from(path::profile(kind))
            .expect("a path built from a ProfileKind is always valid");

        Self::builder(connection)
            .path(path)
            .map_err(crate::Error::Bus)?
            .build()
            .await
            .map_err(crate::Error::Bus)
    }
}
