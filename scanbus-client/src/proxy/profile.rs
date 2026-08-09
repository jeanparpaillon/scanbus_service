//! `org.scanbus.Profile1` — the configurable options of a post-processing profile
//! ([`scanbus-dbus-api.md`] §6).
//!
//! # This one is narrower than the others, on purpose
//!
//! §6 names the objects and gives a table of "typical options" per profile — format and
//! quality for `image`, language for `ocr` — but never fixes an interface: the options
//! are per-profile and open, which is exactly what an `a{sv}` is for. So the proxy is
//! `Name` plus `Options`, which is what `profile list`, `profile show` and `profile set`
//! ([`scanbus-cli.md`] §3) need and no more. [3.1] is what implements the objects, and if
//! it finds it needs a typed property per profile, this is where that lands — adding one
//! now would be inventing a contract from the client side.
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
