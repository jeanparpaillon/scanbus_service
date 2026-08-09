//! `org.scanbus.Job1` — one scan in flight ([`scanbus-dbus-api.md`] §4).
//!
//! Transient by construction: the object is created when data starts arriving and
//! removed when the pipeline is done, so **every call here can come back as
//! `org.freedesktop.DBus.Error.UnknownObject`** and that is not an error condition of the
//! API — it is the job having finished. A client that wants to observe a job reliably
//! follows `InterfacesAdded` and `PropertiesChanged` rather than polling these getters
//! ([`scanbus-cli.md`] §11.3).
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

use std::collections::HashMap;

use scanbus_core::{ScannerId, path};
use zbus::proxy;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

/// A scan in progress or just finished.
#[proxy(interface = "org.scanbus.Job1", default_service = "org.scanbus")]
pub trait Job1 {
    /// Path of the `Scanner1` this job came from.
    #[zbus(property)]
    fn scanner(&self) -> zbus::Result<OwnedObjectPath>;

    /// Index of the physical key that started it, `-1` when triggered by `Scan()`.
    ///
    /// Signed for exactly that reason, so `-1` is a value rather than a wrap-around.
    #[zbus(property)]
    fn button(&self) -> zbus::Result<i32>;

    /// Profile applied, `""` for raw — copied from `Button1.Profile` at trigger time.
    #[zbus(property)]
    fn profile(&self) -> zbus::Result<String>;

    /// `"receiving"` → `"processing"` → `"done"` / `"error"`.
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;

    /// Pages received so far; grows during an ADF run.
    #[zbus(property)]
    fn page_count(&self) -> zbus::Result<u32>;

    /// Profile-specific outcome — `{"path": s}` for `document`, `{"paths": as}` for
    /// `image`, and so on (§6). Empty until `State="done"`.
    ///
    /// [`crate::convert::from_dict`] reads it; there is no typed model for it, because
    /// the shapes belong to the profile processors of workstream 3.
    #[zbus(property)]
    fn result(&self) -> zbus::Result<HashMap<String, OwnedValue>>;

    /// Error message while `State="error"`, empty otherwise.
    #[zbus(property)]
    fn error(&self) -> zbus::Result<String>;
}

impl<'a> Job1Proxy<'a> {
    /// A proxy for job `job_id` of the scanner `id`, at the path §1 gives it.
    ///
    /// # Errors
    ///
    /// [`Error::Bus`](crate::Error::Bus) if the proxy cannot be built. A job that has
    /// already been removed still builds — the failure comes at the first call.
    pub async fn for_job(
        connection: &zbus::Connection,
        id: &ScannerId,
        job_id: u64,
    ) -> crate::Result<Job1Proxy<'a>> {
        let path = zbus::zvariant::ObjectPath::try_from(path::job(id, job_id))
            .expect("a path built from a ScannerId and a job id is always valid");

        Self::builder(connection)
            .path(path)
            .map_err(crate::Error::Bus)?
            .build()
            .await
            .map_err(crate::Error::Bus)
    }
}
