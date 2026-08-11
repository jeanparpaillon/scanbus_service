//! Selectors against a bus with an object tree on it ([`scanbus-cli.md`] §5).
//!
//! These assertions target selector resolution, not stub plumbing. The fixture therefore
//! exports the minimum surface that today's real commands read after resolution succeeds,
//! so the acceptance remains "success when the selector resolves, exit 4 when it does
//! not" even though several of these commands are no longer stubs.
//!
//! The stand-in is not the daemon — same reason as `status.rs`: what is under test is the
//! client's reading of an object tree, and a real daemon would bring a backend probe and
//! a store into it. Its `Manager1` counts `StartDiscovery` calls, because "resolution
//! never starts discovery" is otherwise an invisible promise.
//!
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{PrivateBus, skipped};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

/// The two Brothers of §5's ambiguity example, on the same subnet.
const BROTHER_23: &str = "brother_net_192_2E168_2E1_2E23";
const BROTHER_24: &str = "brother_net_192_2E168_2E1_2E24";
const ESCL: &str = "escl_avahi_HP_OfficeJet_8010";

/// A `Manager1` that answers, and remembers whether it was asked to go looking.
struct Manager {
    discoveries: Arc<AtomicUsize>,
}

#[zbus::interface(name = "org.scanbus.Manager1")]
impl Manager {
    fn start_discovery(&self, _filters: HashMap<String, OwnedValue>) {
        self.discoveries.fetch_add(1, Ordering::SeqCst);
    }

    fn stop_discovery(&self) {}

    fn get_profile_types(&self) -> Vec<String> {
        vec!["image".to_owned(), "document".to_owned()]
    }
}

/// A `Scanner1` with the minimum surface the selector tests' real commands reach after
/// resolution succeeds.
struct Scanner {
    id: String,
    name: String,
}

#[zbus::interface(name = "org.scanbus.Scanner1")]
impl Scanner {
    #[zbus(property)]
    fn id(&self) -> String {
        self.id.clone()
    }

    #[zbus(property)]
    fn name(&self) -> String {
        self.name.clone()
    }

    #[zbus(property)]
    fn backend(&self) -> String {
        "mock".to_owned()
    }

    #[zbus(property)]
    fn address(&self) -> String {
        format!("mock://{}", self.id)
    }

    #[zbus(property)]
    fn capabilities(&self) -> HashMap<String, OwnedValue> {
        HashMap::new()
    }

    #[zbus(property)]
    fn supported_profiles(&self) -> Vec<String> {
        vec!["image".to_owned(), "document".to_owned()]
    }

    #[zbus(property)]
    fn paired(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn connected(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn status(&self) -> String {
        "online".to_owned()
    }

    #[zbus(property)]
    fn default_profile(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn pairing_state(&self) -> String {
        "none".to_owned()
    }

    #[zbus(property)]
    fn pairing_error(&self) -> String {
        String::new()
    }

    fn connect(&self, _options: HashMap<String, OwnedValue>) {}
}

/// A `Button1` reduced the same way: an index, in the path, and the label §5 matches.
struct Button {
    index: u32,
    device_label: String,
    state: Arc<Mutex<ButtonState>>,
}

#[derive(Default)]
struct ButtonState {
    label: String,
    profile: String,
    profile_options: HashMap<String, OwnedValue>,
}

#[zbus::interface(name = "org.scanbus.Button1")]
impl Button {
    #[zbus(property)]
    fn index(&self) -> u32 {
        self.index
    }

    #[zbus(property)]
    fn device_label(&self) -> String {
        self.device_label.clone()
    }

    #[zbus(property)]
    fn label_configurable(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn label(&self) -> String {
        let label = self.state.lock().unwrap().label.clone();
        if label.is_empty() {
            self.device_label.clone()
        } else {
            label
        }
    }

    #[zbus(property)]
    fn set_label(&self, _value: &str) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::PropertyReadOnly(
            "fixed device label".to_owned(),
        ))
    }

    #[zbus(property)]
    fn profile(&self) -> String {
        self.state.lock().unwrap().profile.clone()
    }

    #[zbus(property)]
    fn set_profile(&self, value: &str) {
        self.state.lock().unwrap().profile = value.to_owned();
    }

    #[zbus(property)]
    fn profile_options(&self) -> HashMap<String, OwnedValue> {
        self.state.lock().unwrap().profile_options.clone()
    }

    #[zbus(property)]
    fn set_profile_options(&self, value: HashMap<String, OwnedValue>) {
        self.state.lock().unwrap().profile_options = value;
    }
}

/// A `Job1`, which a selector reaches by its path alone.
struct Job {
    scanner: String,
}

#[zbus::interface(name = "org.scanbus.Job1")]
impl Job {
    #[zbus(property)]
    fn scanner(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(format!("/org/scanbus/scanner/{}", self.scanner)).unwrap()
    }

    #[zbus(property)]
    fn button(&self) -> i32 {
        2
    }

    #[zbus(property)]
    fn profile(&self) -> String {
        "document".to_owned()
    }

    #[zbus(property)]
    fn state(&self) -> String {
        "receiving".to_owned()
    }

    #[zbus(property)]
    fn page_count(&self) -> u32 {
        1
    }

    #[zbus(property)]
    fn result(&self) -> HashMap<String, OwnedValue> {
        HashMap::new()
    }

    #[zbus(property)]
    fn error(&self) -> String {
        String::new()
    }
}

/// Owns `org.scanbus` on `address`, exporting the fixture tree, until dropped.
///
/// Three scanners — two whose ids differ only in their last character and whose names
/// both contain `MFC`, one that is unlike both — a four-key menu on the first, and a job
/// numbered 7 on each of the two Brothers so that a short job id is ambiguous.
async fn serve(address: &str, discoveries: Arc<AtomicUsize>) -> zbus::Connection {
    let connection = zbus::connection::Builder::address(address)
        .expect("the private bus address must parse")
        .name("org.scanbus")
        .expect("org.scanbus is a well-known name")
        .serve_at("/org/scanbus", Manager { discoveries })
        .expect("/org/scanbus is a valid path")
        // Without this there is no GetManagedObjects at all, and every resolution would
        // fail for a reason that has nothing to do with the selector.
        .serve_at("/org/scanbus", zbus::fdo::ObjectManager)
        .expect("/org/scanbus is a valid path")
        .build()
        .await
        .expect("cannot own org.scanbus on the private bus");

    let server = connection.object_server();

    for (id, name) in [
        (BROTHER_23, "MFC-L2710DW"),
        (BROTHER_24, "MFC-J5330DW"),
        (ESCL, "HP OfficeJet 8010"),
    ] {
        server
            .at(
                format!("/org/scanbus/scanner/{id}"),
                Scanner {
                    id: id.to_owned(),
                    name: name.to_owned(),
                },
            )
            .await
            .expect("cannot export the scanner");
    }

    for (index, label) in [
        (0, "Scan to File"),
        (1, "Scan to Image"),
        (2, "Scan to OCR"),
        (3, "Scan to E-mail"),
    ] {
        let state = Arc::new(Mutex::new(ButtonState {
            label: String::new(),
            profile: if index == 2 {
                "document".to_owned()
            } else {
                String::new()
            },
            profile_options: HashMap::new(),
        }));
        server
            .at(
                format!("/org/scanbus/scanner/{BROTHER_23}/button/{index}"),
                Button {
                    index,
                    device_label: label.to_owned(),
                    state,
                },
            )
            .await
            .expect("cannot export the button");
    }

    for (path, scanner) in [
        (
            format!("/org/scanbus/scanner/{BROTHER_23}/job/7"),
            BROTHER_23,
        ),
        (
            format!("/org/scanbus/scanner/{BROTHER_23}/job/8"),
            BROTHER_23,
        ),
        (
            format!("/org/scanbus/scanner/{BROTHER_24}/job/7"),
            BROTHER_24,
        ),
    ] {
        server
            .at(
                path,
                Job {
                    scanner: scanner.to_owned(),
                },
            )
            .await
            .expect("cannot export the job");
    }

    connection
}

/// A bus with the fixture tree on it, or `None` when `dbus-daemon` is missing.
async fn fixture() -> Option<(PrivateBus, zbus::Connection, Arc<AtomicUsize>)> {
    let bus = PrivateBus::start()?;
    let discoveries = Arc::new(AtomicUsize::new(0));
    let daemon = serve(bus.address(), Arc::clone(&discoveries)).await;

    Some((bus, daemon, discoveries))
}

/// Acceptance: with two scanners whose ids share a prefix, that prefix exits 4 and prints
/// both ids; the full id of either works.
#[tokio::test(flavor = "multi_thread")]
async fn a_shared_id_prefix_exits_four_and_the_full_id_of_either_works() {
    let Some((bus, _daemon, _)) = fixture().await else {
        return skipped("a_shared_id_prefix_exits_four_and_the_full_id_of_either_works");
    };

    let run = bus.scanbus(&["connect", "brother"]);
    run.assert_code(4);
    assert!(run.stderr.contains(BROTHER_23), "{}", run.stderr);
    assert!(run.stderr.contains(BROTHER_24), "{}", run.stderr);
    assert!(run.stderr.contains("use the full id"), "{}", run.stderr);
    assert!(run.stdout.is_empty(), "errors go to stderr: {}", run.stdout);

    for full in [BROTHER_23, BROTHER_24] {
        let run = bus.scanbus(&["connect", full]);
        run.assert_code(0);
    }
}

/// Acceptance: a `Name` substring matching one scanner resolves; matching two exits 4.
#[tokio::test(flavor = "multi_thread")]
async fn a_name_substring_resolves_when_unique_and_exits_four_when_not() {
    let Some((bus, _daemon, _)) = fixture().await else {
        return skipped("a_name_substring_resolves_when_unique_and_exits_four_when_not");
    };

    bus.scanbus(&["connect", "l2710"]).assert_code(0);
    bus.scanbus(&["connect", "officejet"]).assert_code(0);

    let run = bus.scanbus(&["connect", "MFC"]);
    run.assert_code(4);
    assert!(run.stderr.contains("MFC-L2710DW"), "{}", run.stderr);
    assert!(run.stderr.contains("MFC-J5330DW"), "{}", run.stderr);
}

/// Acceptance: `--id MFC` fails even when `MFC` is an unambiguous name substring.
#[tokio::test(flavor = "multi_thread")]
async fn the_id_flag_refuses_a_name_that_would_otherwise_resolve() {
    let Some((bus, _daemon, _)) = fixture().await else {
        return skipped("the_id_flag_refuses_a_name_that_would_otherwise_resolve");
    };

    // Unambiguous as a name substring, and still not an id.
    bus.scanbus(&["connect", "l2710"]).assert_code(0);

    let run = bus.scanbus(&["connect", "--id", "l2710"]);
    run.assert_code(4);
    assert!(run.stderr.contains("no scanner matches"), "{}", run.stderr);

    // A unique id prefix and the object path are refused by `--id` too; the id is not.
    bus.scanbus(&["connect", "--id", "escl_avahi"])
        .assert_code(4);
    bus.scanbus(&["connect", "--id", &format!("/org/scanbus/scanner/{ESCL}")])
        .assert_code(4);
    bus.scanbus(&["connect", "--id", ESCL]).assert_code(0);
}

/// A selector matching nothing is exit 4 and lists what does exist — not exit 1, which
/// is what an unfinished command answers.
#[tokio::test(flavor = "multi_thread")]
async fn a_selector_matching_nothing_exits_four_and_lists_the_known_ids() {
    let Some((bus, _daemon, _)) = fixture().await else {
        return skipped("a_selector_matching_nothing_exits_four_and_lists_the_known_ids");
    };

    let run = bus.scanbus(&["unpair", "epson", "--yes"]);
    run.assert_code(4);
    assert!(run.stderr.contains("no scanner matches"), "{}", run.stderr);
    assert!(run.stderr.contains("known scanners:"), "{}", run.stderr);
    assert!(run.stderr.contains(ESCL), "{}", run.stderr);
}

/// An object path resolves, and one that names nothing is still exit 4.
#[tokio::test(flavor = "multi_thread")]
async fn an_object_path_resolves_and_a_wrong_one_does_not() {
    let Some((bus, _daemon, _)) = fixture().await else {
        return skipped("an_object_path_resolves_and_a_wrong_one_does_not");
    };

    bus.scanbus(&["connect", &format!("/org/scanbus/scanner/{ESCL}")])
        .assert_code(0);
    bus.scanbus(&["connect", "/org/scanbus/scanner/nothing"])
        .assert_code(4);
}

/// Buttons: an index, a unique label substring, and the two ways of getting it wrong.
#[tokio::test(flavor = "multi_thread")]
async fn a_button_resolves_by_index_or_label_and_refuses_an_ambiguous_one() {
    let Some((bus, _daemon, _)) = fixture().await else {
        return skipped("a_button_resolves_by_index_or_label_and_refuses_an_ambiguous_one");
    };

    let run = bus.scanbus(&["button", "set", "l2710", "2", "--profile", "document"]);
    run.assert_code(0);
    bus.scanbus(&["button", "clear", "l2710", "E-mail"])
        .assert_code(0);

    // "Scan to" is every key of this menu.
    let run = bus.scanbus(&["button", "clear", "l2710", "Scan to"]);
    run.assert_code(4);
    assert!(run.stderr.contains("Scan to OCR"), "{}", run.stderr);
    assert!(
        run.stderr.contains("use the button's index"),
        "{}",
        run.stderr
    );

    // An index this menu does not have, listed against the four it does.
    let run = bus.scanbus(&["button", "clear", "l2710", "9"]);
    run.assert_code(4);
    assert!(run.stderr.contains("known buttons:"), "{}", run.stderr);

    // A scanner with no buttons exported at all: the button fails, not the scanner.
    let run = bus.scanbus(&["button", "clear", ESCL, "0"]);
    run.assert_code(4);
    assert!(
        run.stderr.contains("exports no buttons right now"),
        "{}",
        run.stderr
    );
}

/// Jobs: the short id is the last path element, and it is unique per scanner only.
#[tokio::test(flavor = "multi_thread")]
async fn a_job_resolves_by_short_id_unless_two_scanners_share_it() {
    let Some((bus, _daemon, _)) = fixture().await else {
        return skipped("a_job_resolves_by_short_id_unless_two_scanners_share_it");
    };

    let run = bus.scanbus(&["job", "show", "8"]);
    run.assert_code(0);

    let run = bus.scanbus(&["job", "show", "7"]);
    run.assert_code(4);
    assert!(
        run.stderr
            .contains(&format!("/org/scanbus/scanner/{BROTHER_23}/job/7")),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("use the full object path"),
        "{}",
        run.stderr
    );

    bus.scanbus(&[
        "job",
        "show",
        &format!("/org/scanbus/scanner/{BROTHER_24}/job/7"),
    ])
    .assert_code(0);
}

/// The `--scanner` of a listing is a selector like any other, and `--id` pins it.
#[tokio::test(flavor = "multi_thread")]
async fn a_job_listing_resolves_its_scanner_filter() {
    let Some((bus, _daemon, _)) = fixture().await else {
        return skipped("a_job_listing_resolves_its_scanner_filter");
    };

    bus.scanbus(&["job", "list", "--scanner", "l2710"])
        .assert_code(0);
    bus.scanbus(&["job", "list", "--scanner", "MFC"])
        .assert_code(4);
    bus.scanbus(&["job", "list", "--scanner", "l2710", "--id"])
        .assert_code(4);

    // No filter, nothing to resolve: a listing must not fail for want of a selector.
    bus.scanbus(&["job", "list"]).assert_code(0);

    // And `--id` with nothing to qualify is a usage error, before any bus traffic.
    bus.scanbus(&["job", "list", "--id"]).assert_code(2);
}

/// Acceptance: resolution reads the tree and never goes looking.
///
/// `StartDiscovery` on this daemon increments a counter, and none of the commands
/// below — resolving, failing to resolve, or listing — may touch it. `discover` is where
/// starting a session belongs, and it is still a stub.
#[tokio::test(flavor = "multi_thread")]
async fn resolution_never_starts_discovery() {
    let Some((bus, _daemon, discoveries)) = fixture().await else {
        return skipped("resolution_never_starts_discovery");
    };

    for args in [
        vec!["connect", "l2710"],
        vec!["connect", "MFC"],
        vec!["connect", "epson"],
        vec!["pair", BROTHER_23],
        vec!["button", "clear", "l2710", "0"],
        vec!["job", "show", "8"],
        vec!["list"],
    ] {
        bus.scanbus(&args);
    }

    assert_eq!(
        discoveries.load(Ordering::SeqCst),
        0,
        "resolution started a discovery session"
    );
}
