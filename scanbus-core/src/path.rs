//! Object paths, built and read in one place.
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
//! # Why this is in core rather than beside the object server
//!
//! Three crates need the same answer: the daemon exports at these paths, the client
//! builds a proxy for a scanner it only knows the id of, and both have to read an id
//! back out of a path that arrived in an `InterfacesAdded`. The daemon had it first, and
//! a client crate reaching *up* into `scanbus-daemon` for it is the dependency edge
//! [`scanbus-cli.md`] §2 rules out — so it moves down here, where neither direction is
//! uphill.
//!
//! Paths are [`String`]s here because core must build without a bus, so
//! [`zvariant::ObjectPath`] is not available to it. The daemon wraps each of these in
//! one — see `scanbus_daemon::dbus::path` — and that wrap is infallible: every
//! constructor below composes literals with a [`ScannerId`], whose charset invariant is
//! exactly the D-Bus path charset, a `u32`, a `u64` or one of four lowercase names, so
//! no combination can produce a path the bus would reject. The longest one is
//! `21 + 200 + 5 + 20 = 246` bytes, inside D-Bus's 255-byte limit — which is what
//! [`ScannerId::MAX_LEN`] was chosen for.
//!
//! # Parsing is not the inverse of a `Display`
//!
//! [`scanner_id`] and friends match the *whole* path and nothing else: a `Button1` path
//! is not a `Scanner1` path with something after it, because a client that treats it as
//! one publishes a scanner per button. Use [`owning_scanner`] to go from a child object
//! to the scanner it belongs to.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

use crate::model::{ProfileKind, ScannerId};

/// The root of the tree: `Manager1` and `ObjectManager` live here.
pub const ROOT: &str = "/org/scanbus";

/// The path element that introduces a scanner.
const SCANNER: &str = "scanner";
/// The path element that introduces a button under a scanner.
const BUTTON: &str = "button";
/// The path element that introduces a job under a scanner.
const JOB: &str = "job";
/// The path element that introduces a profile.
const PROFILE: &str = "profile";

/// `/org/scanbus` — the manager, and the `ObjectManager` every client subscribes to.
pub fn manager() -> String {
    ROOT.to_owned()
}

/// `/org/scanbus/scanner/{id}`.
pub fn scanner(id: &ScannerId) -> String {
    format!("{ROOT}/{SCANNER}/{id}")
}

/// `/org/scanbus/scanner/{id}/button/{index}` — one per entry of the physical menu.
pub fn button(id: &ScannerId, index: u32) -> String {
    format!("{ROOT}/{SCANNER}/{id}/{BUTTON}/{index}")
}

/// `/org/scanbus/scanner/{id}/job/{job_id}` — transient, for one scan in flight.
pub fn job(id: &ScannerId, job_id: u64) -> String {
    format!("{ROOT}/{SCANNER}/{id}/{JOB}/{job_id}")
}

/// `/org/scanbus/profile/{name}`.
pub fn profile(kind: ProfileKind) -> String {
    format!("{ROOT}/{PROFILE}/{kind}")
}

/// The scanner a `Scanner1` path names, or `None` for any other path.
pub fn scanner_id(path: &str) -> Option<ScannerId> {
    match *elements(path)?.as_slice() {
        [SCANNER, id] => ScannerId::new(id).ok(),
        _ => None,
    }
}

/// The scanner and index a `Button1` path names.
pub fn button_index(path: &str) -> Option<(ScannerId, u32)> {
    match *elements(path)?.as_slice() {
        [SCANNER, id, BUTTON, index] => Some((ScannerId::new(id).ok()?, index.parse().ok()?)),
        _ => None,
    }
}

/// The scanner and job id a `Job1` path names.
pub fn job_id(path: &str) -> Option<(ScannerId, u64)> {
    match *elements(path)?.as_slice() {
        [SCANNER, id, JOB, job] => Some((ScannerId::new(id).ok()?, job.parse().ok()?)),
        _ => None,
    }
}

/// The profile a `Profile1` path names.
pub fn profile_kind(path: &str) -> Option<ProfileKind> {
    match *elements(path)?.as_slice() {
        [PROFILE, name] => name.parse().ok(),
        _ => None,
    }
}

/// The scanner an object belongs to: itself for a `Scanner1` path, the parent for a
/// `Button1` or `Job1` one.
///
/// What a client reaches for having received a `PropertiesChanged` on a job and wanting
/// the scanner it came from, rather than re-reading `Job1.Scanner` off the bus.
pub fn owning_scanner(path: &str) -> Option<ScannerId> {
    match *elements(path)?.as_slice() {
        [SCANNER, id] | [SCANNER, id, BUTTON | JOB, _] => ScannerId::new(id).ok(),
        _ => None,
    }
}

/// The path elements below [`ROOT`], or `None` if the path is not under it at all.
///
/// A path *equal* to `ROOT` yields an empty slice, which no pattern above matches — the
/// manager is not one of the object kinds these functions identify.
fn elements(path: &str) -> Option<Vec<&str>> {
    if path == ROOT {
        return Some(Vec::new());
    }

    let rest = path.strip_prefix(ROOT)?.strip_prefix('/')?;
    // An empty element means a doubled or trailing slash, which is not a legal object
    // path — refusing here keeps `//org/scanbus/scanner//x` from parsing as an id.
    if rest.split('/').any(str::is_empty) {
        return None;
    }

    Some(rest.split('/').collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The id from §1 is a SANE device URI, i.e. dots and colons — the case that would
    /// be rejected by the bus at export time if the escaping in
    /// [`ScannerId::from_backend`] ever stopped covering it.
    fn sane_id() -> ScannerId {
        ScannerId::from_backend("sane", "epson2:net:192.168.1.50").unwrap()
    }

    #[test]
    fn a_sane_device_uri_gives_the_documented_paths() {
        let id = sane_id();
        assert_eq!(id.as_str(), "sane_epson2_3Anet_3A192_2E168_2E1_2E50");

        assert_eq!(
            scanner(&id),
            "/org/scanbus/scanner/sane_epson2_3Anet_3A192_2E168_2E1_2E50"
        );
        assert_eq!(
            button(&id, 3),
            "/org/scanbus/scanner/sane_epson2_3Anet_3A192_2E168_2E1_2E50/button/3"
        );
        assert_eq!(
            job(&id, 7),
            "/org/scanbus/scanner/sane_epson2_3Anet_3A192_2E168_2E1_2E50/job/7"
        );
        assert_eq!(profile(ProfileKind::Ocr), "/org/scanbus/profile/ocr");
    }

    /// Every child path is under the manager, which is what makes `InterfacesAdded`
    /// reach a client that only subscribed to `/org/scanbus`.
    #[test]
    fn every_object_lives_under_the_manager() {
        let id = sane_id();
        assert_eq!(manager(), ROOT);

        for path in [
            scanner(&id),
            button(&id, 0),
            job(&id, 0),
            profile(ProfileKind::Document),
        ] {
            assert!(
                path.starts_with(&format!("{ROOT}/")),
                "{path} is not under the manager"
            );
        }
    }

    #[test]
    fn profiles_use_the_documented_names() {
        for kind in ProfileKind::ALL {
            assert_eq!(profile(kind), format!("/org/scanbus/profile/{kind}"));
            assert_eq!(profile_kind(&profile(kind)), Some(kind));
        }
    }

    /// The bound that makes wrapping these in an `ObjectPath` infallible: the longest
    /// path an id can appear in stays inside D-Bus's 255-byte limit.
    #[test]
    fn the_longest_possible_path_still_fits() {
        let id = ScannerId::new("a".repeat(ScannerId::MAX_LEN)).unwrap();

        assert!(
            job(&id, u64::MAX).len() <= 255,
            "{} bytes is over the D-Bus limit",
            job(&id, u64::MAX).len()
        );
        assert!(button(&id, u32::MAX).len() <= 255);
    }

    /// Distinct scanners, buttons and jobs never share a path — the registry keys on
    /// the path, so a collision would be two objects overwriting each other.
    #[test]
    fn distinct_objects_give_distinct_paths() {
        let one = ScannerId::from_backend("sane", "epson2:net:192.168.1.50").unwrap();
        let two = ScannerId::from_backend("sane", "epson2:net:192.168.1.5").unwrap();

        let mut paths = vec![
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
        ];

        let count = paths.len();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(paths.len(), count, "two objects collided onto one path");
    }

    #[test]
    fn every_constructor_round_trips_through_its_parser() {
        let id = sane_id();

        assert_eq!(scanner_id(&scanner(&id)), Some(id.clone()));
        assert_eq!(button_index(&button(&id, 3)), Some((id.clone(), 3)));
        assert_eq!(job_id(&job(&id, u64::MAX)), Some((id.clone(), u64::MAX)));
        assert_eq!(
            profile_kind(&profile(ProfileKind::Email)),
            Some(ProfileKind::Email)
        );
    }

    /// A parser answering for a path of another kind is how a client ends up with one
    /// scanner per button.
    #[test]
    fn each_parser_refuses_the_other_kinds_of_path() {
        let id = sane_id();
        let scanner_path = scanner(&id);
        let button_path = button(&id, 0);
        let job_path = job(&id, 0);
        let profile_path = profile(ProfileKind::Image);

        assert_eq!(scanner_id(&button_path), None);
        assert_eq!(scanner_id(&job_path), None);
        assert_eq!(scanner_id(&profile_path), None);
        assert_eq!(scanner_id(ROOT), None);

        assert_eq!(button_index(&scanner_path), None);
        assert_eq!(button_index(&job_path), None);
        assert_eq!(job_id(&button_path), None);
        assert_eq!(profile_kind(&scanner_path), None);
    }

    /// Paths that are not ours at all, and malformed ones.
    #[test]
    fn foreign_and_malformed_paths_parse_as_nothing() {
        for path in [
            "/org/freedesktop/UPower/devices/battery_BAT0",
            "/org/scanbusier/scanner/mock_x",
            "/org/scanbus/scanner",
            "/org/scanbus/scanner/",
            "/org/scanbus/scanner//mock_x",
            "/org/scanbus/scanner/mock_x/button/",
            "/org/scanbus/scanner/mock_x/button/beta",
            "/org/scanbus/scanner/mock-x",
            "",
        ] {
            assert_eq!(scanner_id(path), None, "{path} parsed as a scanner");
            assert_eq!(button_index(path), None, "{path} parsed as a button");
            assert_eq!(job_id(path), None, "{path} parsed as a job");
        }
    }

    /// A job id larger than a `u64` is a daemon we do not understand, not a job 0.
    #[test]
    fn an_out_of_range_index_is_not_silently_truncated() {
        let id = sane_id();
        assert_eq!(
            job_id(&format!("{ROOT}/scanner/{id}/job/18446744073709551616")),
            None
        );
        assert_eq!(
            button_index(&format!("{ROOT}/scanner/{id}/button/-1")),
            None
        );
    }

    #[test]
    fn a_child_object_names_the_scanner_that_owns_it() {
        let id = sane_id();

        assert_eq!(owning_scanner(&scanner(&id)), Some(id.clone()));
        assert_eq!(owning_scanner(&button(&id, 2)), Some(id.clone()));
        assert_eq!(owning_scanner(&job(&id, 9)), Some(id.clone()));
        assert_eq!(owning_scanner(&profile(ProfileKind::Image)), None);
        assert_eq!(owning_scanner(ROOT), None);
    }
}
