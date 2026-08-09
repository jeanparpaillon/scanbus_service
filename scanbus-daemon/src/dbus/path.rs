//! The object tree of [`scanbus-dbus-api.md`] §1, as [`OwnedObjectPath`]s.
//!
//! The paths themselves are built by [`scanbus_core::path`], because the client crate
//! needs the same ones and must not depend on this one ([`scanbus-cli.md`] §2). What is
//! left here is the wrap into the bus's type, which is infallible for everything core
//! composes — see that module for why — so these constructors stay panic-free without
//! returning a `Result` nobody could act on.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

use scanbus_core::{ProfileKind, ScannerId, path};
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

/// The root of the tree: `Manager1` and `ObjectManager` live here.
pub const ROOT: &str = path::ROOT;

/// Wraps a path core built.
///
/// The panic is unreachable for every input the constructors below accept — see
/// [`scanbus_core::path`] — so a failure here means one of those invariants has been
/// weakened, which is worth failing loudly rather than turning into an `InvalidArgs`
/// from the bus at export time.
fn build(path: String) -> OwnedObjectPath {
    ObjectPath::try_from(path)
        .expect("path built from a ScannerId, an index and a literal is always valid")
        .into()
}

/// `/org/scanbus` — the manager, and the `ObjectManager` every client subscribes to.
pub fn manager() -> OwnedObjectPath {
    build(path::manager())
}

/// `/org/scanbus/scanner/{id}`.
pub fn scanner(id: &ScannerId) -> OwnedObjectPath {
    build(path::scanner(id))
}

/// `/org/scanbus/scanner/{id}/button/{index}` — one per entry of the physical menu.
pub fn button(id: &ScannerId, index: u32) -> OwnedObjectPath {
    build(path::button(id, index))
}

/// `/org/scanbus/scanner/{id}/job/{job_id}` — transient, for one scan in flight.
pub fn job(id: &ScannerId, job_id: u64) -> OwnedObjectPath {
    build(path::job(id, job_id))
}

/// `/org/scanbus/profile/{name}`.
pub fn profile(kind: ProfileKind) -> OwnedObjectPath {
    build(path::profile(kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The composition itself is core's, and tested there. What has to hold here is
    /// that the bus accepts every path it produces — the one thing core cannot check,
    /// since it has no `ObjectPath` to try.
    #[test]
    fn every_path_core_builds_is_one_the_bus_accepts() {
        let longest = ScannerId::new("a".repeat(ScannerId::MAX_LEN)).unwrap();
        let awkward = ScannerId::from_backend("sane", "epson2:net:192.168.1.50").unwrap();

        for id in [longest, awkward] {
            assert_eq!(scanner(&id).as_str(), path::scanner(&id));
            assert_eq!(button(&id, u32::MAX).as_str(), path::button(&id, u32::MAX));
            assert_eq!(job(&id, u64::MAX).as_str(), path::job(&id, u64::MAX));
        }

        assert_eq!(manager().as_str(), ROOT);
        for kind in ProfileKind::ALL {
            assert_eq!(profile(kind).as_str(), path::profile(kind));
        }
    }
}
