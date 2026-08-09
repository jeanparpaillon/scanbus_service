//! Object paths, built in one place.
//!
//! The tree of [`scanbus-dbus-api.md`] §1:
//!
//! ```text
//! /org/scanbus                              (Manager1, ObjectManager)
//! /org/scanbus/scanner/{id}                 (Scanner1)
//! /org/scanbus/scanner/{id}/button/{n}      (Button1)
//! /org/scanbus/scanner/{id}/job/{jobid}     (Job1)
//! /org/scanbus/profile/{name}               (Profile1)
//! ```
//!
//! Every constructor here is infallible, and that is the point of
//! [`ScannerId`](scanbus_core::ScannerId) having a charset invariant at all: `{id}` is
//! already `[A-Za-z0-9_]` and at most [`ScannerId::MAX_LEN`] bytes, `{n}` is a `u32`,
//! `{jobid}` a `u64` and `{name}` one of four lowercase literals, so no combination can
//! produce a path the bus would reject. The longest one this can build is
//! `21 + 200 + 5 + 20 = 246` bytes, inside D-Bus's 255-byte limit — which is what
//! `MAX_LEN` was chosen for.
//!
//! Job ids are `u64` because the daemon mints them (2.6 requires them unique per
//! scanner and never reused within a daemon lifetime, which a counter gives for free)
//! and because the CLI wants to print a short one ([`scanbus-cli.md`] §5). Nothing
//! outside this daemon ever supplies one, so unlike `{id}` it needs no escaping.
//!
//! [`ScannerId::MAX_LEN`]: scanbus_core::ScannerId::MAX_LEN
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

use scanbus_core::{ProfileKind, ScannerId};
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

/// The root of the tree: `Manager1` and `ObjectManager` live here.
pub const ROOT: &str = "/org/scanbus";

/// Wraps a path this module built itself.
///
/// The panic is unreachable for every input the constructors below accept — see the
/// module documentation — so a failure here means one of those invariants has been
/// weakened, which is worth failing loudly rather than turning into an `InvalidArgs`
/// from the bus at export time.
fn build(path: String) -> OwnedObjectPath {
    ObjectPath::try_from(path)
        .expect("path built from a ScannerId, an index and a literal is always valid")
        .into()
}

/// `/org/scanbus` — the manager, and the `ObjectManager` every client subscribes to.
pub fn manager() -> OwnedObjectPath {
    build(ROOT.to_owned())
}

/// `/org/scanbus/scanner/{id}`.
pub fn scanner(id: &ScannerId) -> OwnedObjectPath {
    build(format!("{ROOT}/scanner/{id}"))
}

/// `/org/scanbus/scanner/{id}/button/{index}` — one per entry of the physical menu.
pub fn button(id: &ScannerId, index: u32) -> OwnedObjectPath {
    build(format!("{ROOT}/scanner/{id}/button/{index}"))
}

/// `/org/scanbus/scanner/{id}/job/{job_id}` — transient, for one scan in flight.
pub fn job(id: &ScannerId, job_id: u64) -> OwnedObjectPath {
    build(format!("{ROOT}/scanner/{id}/job/{job_id}"))
}

/// `/org/scanbus/profile/{name}`.
pub fn profile(kind: ProfileKind) -> OwnedObjectPath {
    build(format!("{ROOT}/profile/{kind}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The id from §1 is a SANE device URI, i.e. dots and colons — the case that would
    /// be an `InvalidArgs` at export time if the escaping in
    /// [`ScannerId::from_backend`] ever stopped covering it.
    fn sane_id() -> ScannerId {
        ScannerId::from_backend("sane", "epson2:net:192.168.1.50").unwrap()
    }

    #[test]
    fn a_sane_device_uri_gives_paths_the_bus_accepts() {
        let id = sane_id();
        assert_eq!(id.as_str(), "sane_epson2_3Anet_3A192_2E168_2E1_2E50");

        assert_eq!(
            scanner(&id).as_str(),
            "/org/scanbus/scanner/sane_epson2_3Anet_3A192_2E168_2E1_2E50"
        );
        assert_eq!(
            button(&id, 3).as_str(),
            "/org/scanbus/scanner/sane_epson2_3Anet_3A192_2E168_2E1_2E50/button/3"
        );
        assert_eq!(
            job(&id, 7).as_str(),
            "/org/scanbus/scanner/sane_epson2_3Anet_3A192_2E168_2E1_2E50/job/7"
        );
    }

    /// Every child path is under the manager, which is what makes `InterfacesAdded`
    /// reach a client that only subscribed to `/org/scanbus`.
    #[test]
    fn every_object_lives_under_the_manager() {
        let id = sane_id();
        let manager = manager();
        assert_eq!(manager.as_str(), ROOT);

        for path in [
            scanner(&id),
            button(&id, 0),
            job(&id, 0),
            profile(ProfileKind::Document),
        ] {
            assert!(
                path.as_str().starts_with(&format!("{ROOT}/")),
                "{path} is not under the manager"
            );
        }
    }

    #[test]
    fn profiles_use_the_documented_names() {
        for kind in ProfileKind::ALL {
            assert_eq!(
                profile(kind).as_str(),
                format!("/org/scanbus/profile/{}", kind.as_str())
            );
        }
    }

    /// The bound that makes every constructor here infallible: the longest path an id
    /// can appear in stays inside D-Bus's 255-byte limit.
    #[test]
    fn the_longest_possible_path_still_fits() {
        let id = ScannerId::new("a".repeat(ScannerId::MAX_LEN)).unwrap();

        let longest = job(&id, u64::MAX);
        assert!(
            longest.as_str().len() <= 255,
            "{} bytes is over the D-Bus limit",
            longest.as_str().len()
        );
        assert!(button(&id, u32::MAX).as_str().len() <= 255);
    }

    /// Distinct scanners, buttons and jobs never share a path — the registry keys on
    /// the path, so a collision would be two objects overwriting each other.
    #[test]
    fn distinct_objects_give_distinct_paths() {
        let one = ScannerId::from_backend("sane", "epson2:net:192.168.1.50").unwrap();
        let two = ScannerId::from_backend("sane", "epson2:net:192.168.1.5").unwrap();

        let mut paths: Vec<String> = [
            manager(),
            scanner(&one),
            scanner(&two),
            button(&one, 0),
            button(&one, 1),
            button(&two, 0),
            job(&one, 0),
            job(&one, 1),
            job(&two, 0),
            profile(ProfileKind::Image),
            profile(ProfileKind::Document),
        ]
        .iter()
        .map(|path| path.as_str().to_owned())
        .collect();

        let count = paths.len();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(paths.len(), count, "two objects collided onto one path");
    }
}
