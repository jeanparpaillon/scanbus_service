//! The contract of [`scanbus-dbus-api.md`], run against the daemon on a private bus.
//!
//! The other suites in this directory each own one mechanism — the object tree, pairing,
//! buttons, jobs. This one asserts the things that are only true of the *whole* service,
//! and that no unit test can see because they only exist on a bus:
//!
//! - **introspection XML** — every member §2–§6 documents is served, on a live object,
//!   under the interface it belongs to. A property renamed in the daemon and not in the
//!   doc is a client that compiles and then finds nothing.
//! - **error-name rendering** — §8's names as they arrive in a `MethodError`, which is
//!   the only place the mapping in [`scanbus_daemon::dbus::error`] can be checked end to
//!   end.
//! - **the flow of §7** — `StartDiscovery` → `InterfacesAdded` → `Pair` →
//!   `PropertiesChanged` → `Connect` → set `Button1.Profile` → press → job object →
//!   `State="done"` — driven through `scanbus-client`'s proxies, so an interface change
//!   breaks the client and the daemon in the same build.
//!
//! Two deltas between the doc and what ships, both deliberate and both asserted below
//! rather than assumed:
//!
//! - `Scanner1.Scan()` is marked *(optional)* in §3 and is not implemented, so it is not
//!   in the expected member list.
//! - `Scanner1.PairingInfo` is served and §3 does not list it; it is documented in
//!   `scanbus-mobile-backend.md` §4 instead. Asserted here so that the day §3 grows it,
//!   nothing has to change.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use scanbus_client::proxy::{
    Button1Proxy, Job1Proxy, Manager1Proxy, Profile1Proxy, Scanner1Proxy, object_manager,
};
use scanbus_client::{Error as ClientError, ScanbusError};
use scanbus_core::backend::mock::{MockBackend, MockHandle};
use scanbus_core::{
    ButtonsCapability, Capabilities, ColorMode, PageFormat, ProfileKind, RawPage, ScannerBackend,
    ScannerId, ScannerInfo, Source, Status, Value, path,
};
use scanbus_daemon::dbus::BUS_NAME;
use scanbus_daemon::{Backends, JobRegistry};
use zbus::fdo::IntrospectableProxy;
use zbus::zvariant::{OwnedValue, Value as ZValue};

mod common;
use common::{Daemon, PrivateBus, skipped};

/// How long a signal that should already be on its way is waited for.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(5);

/// `org.scanbus.Manager1` as §2 defines it.
const MANAGER_MEMBERS: &[&str] = &[
    "StartDiscovery",
    "StopDiscovery",
    "GetProfileTypes",
    "Version",
    "Backends",
];

/// `org.scanbus.Scanner1` as §3 defines it, plus the `PairingInfo` of the mobile backend
/// and minus the optional `Scan()`; see the module docs.
const SCANNER_MEMBERS: &[&str] = &[
    "Id",
    "Name",
    "Backend",
    "Address",
    "Capabilities",
    "SupportedProfiles",
    "Paired",
    "Connected",
    "Status",
    "DefaultProfile",
    "PairingState",
    "PairingError",
    "PairingInfo",
    "Pair",
    "CancelPairing",
    "Unpair",
    "Connect",
    "Disconnect",
];

/// `org.scanbus.Button1` as §5 defines it.
const BUTTON_MEMBERS: &[&str] = &[
    "Index",
    "DeviceLabel",
    "LabelConfigurable",
    "Label",
    "Profile",
    "ProfileOptions",
];

/// `org.scanbus.Job1` as §4 defines it.
const JOB_MEMBERS: &[&str] = &[
    "Scanner",
    "Button",
    "Profile",
    "State",
    "PageCount",
    "Result",
    "Error",
];

/// `org.scanbus.Profile1` as §6 defines it.
const PROFILE_MEMBERS: &[&str] = &["Name", "Options", "OptionsSchema"];

/// A scanner with something in every field, so a round trip that drops one is visible.
fn brother() -> ScannerInfo {
    ScannerInfo {
        id: ScannerId::from_backend("mock", "192.168.1.23").unwrap(),
        name: "Brother MFC-L2710DW".to_owned(),
        backend: "proprietary:brother".to_owned(),
        address: "192.168.1.23".to_owned(),
        capabilities: Capabilities {
            resolutions: vec![100, 200, 300, 600],
            color_modes: vec![ColorMode::Color, ColorMode::Gray, ColorMode::Bw],
            sources: vec![Source::Flatbed, Source::Adf],
            duplex: true,
            buttons: ButtonsCapability {
                count: 4,
                label_configurable: false,
                labels: Vec::new(),
            },
            profiles: Vec::new(),
            // The key this version of the client has no field for: it has to survive the
            // trip out through the daemon's renderer and back through the client's.
            extra: [(
                "max_scan_area_mm".to_owned(),
                Value::Array(vec![Value::U64(216), Value::U64(356)]),
            )]
            .into_iter()
            .collect(),
        },
        status: Status::Online,
    }
}

/// The backend the daemon probes, and the handle a test presses keys on.
///
/// Built here rather than through a `backends()` helper because both halves are needed:
/// [`Backends`] goes into the daemon and never comes back out, and a walk-up scan cannot
/// be triggered without the [`MockHandle`].
fn mock(scanners: impl IntoIterator<Item = ScannerInfo>) -> (Backends, MockHandle) {
    let backend = Arc::new(MockBackend::with_scanners(scanners).with_id("mock"));
    let handle = backend.handle();
    (Backends::new([backend as Arc<dyn ScannerBackend>]), handle)
}

/// A minimal valid PGM (P5) payload: one pixel, so the pipeline has something to convert.
fn page(index: u32) -> RawPage {
    RawPage {
        index,
        format: PageFormat::Pnm,
        resolution_dpi: 300,
        data: b"P5\n1 1\n255\n\x42".to_vec(),
    }
}

/// The introspection XML of one object, as `gdbus introspect` would print it.
async fn introspect(client: &zbus::Connection, object: String) -> String {
    IntrospectableProxy::builder(client)
        .destination(BUS_NAME)
        .unwrap()
        .path(object)
        .unwrap()
        .build()
        .await
        .unwrap()
        .introspect()
        .await
        .unwrap()
}

/// Asserts that `interface` is served at this object and declares every one of `members`.
///
/// The XML is sliced down to the one `<interface>` element first, because a member name
/// is only meaningful under its interface: `Profile` exists on `Button1` *and* on `Job1`,
/// and a search over the whole document would let either stand in for the other.
fn assert_declares(xml: &str, interface: &str, members: &[&str]) {
    let opening = format!("<interface name=\"{interface}\">");
    let start = xml
        .find(&opening)
        .unwrap_or_else(|| panic!("{interface} is not served here:\n{xml}"));
    let body = &xml[start..];
    let end = body
        .find("</interface>")
        .unwrap_or_else(|| panic!("{interface} has no closing tag:\n{xml}"));
    let body = &body[..end];

    for member in members {
        assert!(
            body.contains(&format!("name=\"{member}\"")),
            "{interface}.{member} is documented but not declared:\n{body}"
        );
    }
}

/// Starts discovery and waits for the scanner's object to be announced.
async fn discover(client: &zbus::Connection, id: &ScannerId) {
    let manager = object_manager(client).await.unwrap();
    let mut added = manager.receive_interfaces_added().await.unwrap();

    Manager1Proxy::new(client)
        .await
        .unwrap()
        .start_discovery(HashMap::new())
        .await
        .unwrap();

    await_object(&mut added, path::scanner(id)).await;
}

/// Waits for `wanted` on an `InterfacesAdded` stream that was subscribed *before* the
/// call that creates it — the "subscribe, then act" order §7 puts the client in.
async fn await_object(added: &mut zbus::fdo::InterfacesAddedStream, wanted: String) {
    let deadline = tokio::time::Instant::now() + SIGNAL_TIMEOUT;
    loop {
        let signal = tokio::time::timeout_at(deadline, added.next())
            .await
            .unwrap_or_else(|_| panic!("no InterfacesAdded for {wanted} within the timeout"))
            .expect("the signal stream ended");
        if signal.args().unwrap().object_path().as_str() == wanted {
            return;
        }
    }
}

/// Pairs the scanner and waits for §9's asynchronous outcome to land.
async fn pair(scanner: &Scanner1Proxy<'_>) {
    scanner.pair(HashMap::new()).await.unwrap();
    tokio::time::timeout(SIGNAL_TIMEOUT, async {
        while scanner.pairing_state().await.unwrap() != "done" {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the mock backend pairs well within the timeout");
    assert!(scanner.paired().await.unwrap());
}

/// Points the `document` profile at `folder`, so a test's scans never reach `~/Documents`.
///
/// A whole-map write, as §6 requires: the other keys go back with it or they revert.
async fn write_output_folder(client: &zbus::Connection, folder: &std::path::Path) {
    Profile1Proxy::for_profile(client, ProfileKind::Document)
        .await
        .unwrap()
        .set_options(HashMap::from([(
            "output_folder".to_owned(),
            OwnedValue::try_from(ZValue::from(folder.to_string_lossy().to_string())).unwrap(),
        )]))
        .await
        .unwrap();
}

/// The named refusal `call` produced, or a panic naming what came back instead.
fn refusal(result: Result<(), zbus::Error>) -> ScanbusError {
    match ClientError::from(result.expect_err("the call should have been refused")) {
        ClientError::Call(call) => call,
        other => panic!("expected a named refusal, got {other:?}"),
    }
}

/// Acceptance: every member §2–§6 documents is declared on a live object, under its own
/// interface, alongside the standard interfaces a client expects to find there.
///
/// Run against the real tree rather than against golden files: a golden XML would pin the
/// argument names and the doc-comment annotations zbus emits, and would then have to be
/// regenerated for changes that break nothing. What a client actually depends on is the
/// member being *there*, under that interface name.
#[tokio::test]
async fn every_documented_member_is_declared_on_a_live_object() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("every_documented_member_is_declared_on_a_live_object");
    };

    let found = brother();
    let (backends, handle) = mock([found.clone()]);
    let daemon = Daemon::start(&bus, backends).await;
    let client = bus.connect().await;

    // §2: the manager, and the ObjectManager that stands in for a ScannerFound signal.
    let xml = introspect(&client, path::manager()).await;
    assert_declares(&xml, "org.scanbus.Manager1", MANAGER_MEMBERS);
    for standard in [
        "org.freedesktop.DBus.ObjectManager",
        "org.freedesktop.DBus.Introspectable",
        "org.freedesktop.DBus.Properties",
    ] {
        assert!(xml.contains(standard), "{standard} missing from {xml}");
    }

    // §6: the profiles are persistent, so they are introspectable before anything is
    // discovered.
    for kind in [ProfileKind::Image, ProfileKind::Document] {
        let xml = introspect(&client, path::profile(kind)).await;
        assert_declares(&xml, "org.scanbus.Profile1", PROFILE_MEMBERS);
    }

    // §3 and §5: both need an object, and a scanner only has one once it is discovered.
    discover(&client, &found.id).await;
    let xml = introspect(&client, path::scanner(&found.id)).await;
    assert_declares(&xml, "org.scanbus.Scanner1", SCANNER_MEMBERS);
    assert!(
        !xml.contains("name=\"Scan\""),
        "Scan() is optional in §3 and is not implemented; the expected member list says so"
    );

    let xml = introspect(&client, path::button(&found.id, 2)).await;
    assert_declares(&xml, "org.scanbus.Button1", BUTTON_MEMBERS);

    // §4: a job object exists only while a scan does, so one has to be started.
    let scanner = Scanner1Proxy::for_scanner(&client, &found.id)
        .await
        .unwrap();
    pair(&scanner).await;
    scanner.connect(HashMap::new()).await.unwrap();

    let mut added = object_manager(&client)
        .await
        .unwrap()
        .receive_interfaces_added()
        .await
        .unwrap();
    let feed = handle.open_pages(format!("job-{}", JobRegistry::FIRST_ID));
    handle.press_button(&found.id, 2).unwrap();
    feed.page(page(0)).unwrap();
    await_object(&mut added, path::job(&found.id, JobRegistry::FIRST_ID)).await;

    let xml = introspect(&client, path::job(&found.id, JobRegistry::FIRST_ID)).await;
    assert_declares(&xml, "org.scanbus.Job1", JOB_MEMBERS);

    // The capture is abandoned rather than finished: this test is about the XML, and a
    // completed pipeline would write a file it has nothing to say about.
    feed.end();
    daemon.shutdown().await;
}

/// Acceptance: §7's flow, from an empty bus to a finished job, with every step observed
/// through the signal a client would use rather than by polling.
#[tokio::test]
async fn the_documented_flow_runs_from_discovery_to_a_finished_job() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("the_documented_flow_runs_from_discovery_to_a_finished_job");
    };

    let found = brother();
    let (backends, handle) = mock([found.clone()]);
    let daemon = Daemon::start(&bus, backends).await;
    let client = bus.connect().await;

    let out = std::env::temp_dir().join(format!("scanbus-conformance-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    write_output_folder(&client, &out).await;

    // StartDiscovery → InterfacesAdded, and the object arrives unpaired (§1).
    discover(&client, &found.id).await;
    let scanner = Scanner1Proxy::for_scanner(&client, &found.id)
        .await
        .unwrap();
    assert_eq!(scanner.name().await.unwrap(), found.name);
    assert!(!scanner.paired().await.unwrap());
    assert_eq!(scanner.pairing_state().await.unwrap(), "none");

    // Pair → PropertiesChanged. §9's asynchronous contract: the reply says only that the
    // process started, and `Paired` is read at the end.
    pair(&scanner).await;

    // Connect, then the host-side half of §5: key 2 is assigned a profile.
    scanner.connect(HashMap::new()).await.unwrap();
    assert!(scanner.connected().await.unwrap());

    let button = Button1Proxy::for_button(&client, &found.id, 2)
        .await
        .unwrap();
    button.set_profile("document").await.unwrap();
    assert_eq!(button.profile().await.unwrap(), "document");

    // The walk-up press. Subscribed first, pressed second — the job object is announced
    // on the first page, which is well before the reply to anything.
    let mut added = object_manager(&client)
        .await
        .unwrap()
        .receive_interfaces_added()
        .await
        .unwrap();
    let feed = handle.open_pages(format!("job-{}", JobRegistry::FIRST_ID));
    handle.press_button(&found.id, 2).unwrap();
    feed.page(page(0)).unwrap();

    let job_path = path::job(&found.id, JobRegistry::FIRST_ID);
    await_object(&mut added, job_path.clone()).await;

    let job = Job1Proxy::for_job(&client, &found.id, JobRegistry::FIRST_ID)
        .await
        .unwrap();
    assert_eq!(
        job.scanner().await.unwrap().as_str(),
        path::scanner(&found.id)
    );
    assert_eq!(
        job.button().await.unwrap(),
        2,
        "the key that started it (§4)"
    );
    assert_eq!(job.profile().await.unwrap(), "document");
    assert_eq!(job.state().await.unwrap(), "receiving");
    assert_eq!(job.page_count().await.unwrap(), 1);

    // A second sheet: `PageCount` moves, and it moves by signal.
    let mut changed = scanbus_client::proxy::properties(&client, job_path)
        .await
        .unwrap()
        .receive_properties_changed()
        .await
        .unwrap();
    feed.page(page(1)).unwrap();
    assert_eq!(next_u32(&mut changed, "PageCount").await, 2);

    // End of capture → the pipeline → a terminal state carrying its `Result` (§4).
    feed.end();
    assert_eq!(next_string(&mut changed, "State").await, "processing");
    let terminal = next_string(&mut changed, "State").await;
    assert_eq!(
        terminal,
        "done",
        "the job ended in {terminal}: {}",
        job.error().await.unwrap()
    );

    let written = String::try_from(job.result().await.unwrap()["path"].clone()).unwrap();
    assert!(
        written.starts_with(&out.to_string_lossy().to_string()),
        "the document profile wrote outside the folder it was given: {written}"
    );
    assert!(written.ends_with(".pdf"), "document maps to a PDF (§6)");

    daemon.shutdown().await;
    let _ = std::fs::remove_dir_all(&out);
}

/// Acceptance: §8's names arrive intact, on a `MethodError` from a daemon that meant
/// them.
///
/// The mapping itself has a unit test in [`scanbus_daemon::dbus::error`]; what only a bus
/// can show is that the name survives being put on the wire and read back — a refusal
/// that reached a client as a transport failure would pass that unit test and be useless
/// to every frontend.
#[tokio::test]
async fn every_refusal_arrives_under_its_documented_name() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("every_refusal_arrives_under_its_documented_name");
    };

    let found = brother();
    let (backends, _handle) = mock([found.clone()]);
    let daemon = Daemon::start(&bus, backends).await;
    let client = bus.connect().await;
    discover(&client, &found.id).await;

    let scanner = Scanner1Proxy::for_scanner(&client, &found.id)
        .await
        .unwrap();

    // NotPaired: §3's precondition on `Connect`, and on `Unpair`.
    let error = refusal(scanner.connect(HashMap::new()).await);
    assert_eq!(error.name(), "org.scanbus.Error.NotPaired");
    assert!(error.to_string().contains(found.id.as_str()), "{error}");
    assert_eq!(
        refusal(scanner.unpair().await).name(),
        "org.scanbus.Error.NotPaired"
    );

    // UnsupportedProfile: a session profile outside `SupportedProfiles`. Validated before
    // the pairing check, so this is the same refusal a paired scanner would give.
    let ocr = HashMap::from([(
        "profile".to_owned(),
        OwnedValue::try_from(ZValue::from("ocr")).unwrap(),
    )]);
    assert_eq!(
        refusal(scanner.connect(ocr).await).name(),
        "org.scanbus.Error.UnsupportedProfile"
    );

    // InvalidArgs, the standard name rather than an eighth of our own: §3 documents no
    // `Pair()` option, so any key is one.
    let unknown = HashMap::from([(
        "pin".to_owned(),
        OwnedValue::try_from(ZValue::from("123456")).unwrap(),
    )]);
    assert_eq!(
        refusal(scanner.pair(unknown).await).name(),
        "org.freedesktop.DBus.Error.InvalidArgs"
    );

    // AlreadyPaired: §9's idempotency rule, and the case a client must tell apart from
    // "already in progress" because only one of them has a transition still coming.
    pair(&scanner).await;
    assert_eq!(
        refusal(scanner.pair(HashMap::new()).await).name(),
        "org.scanbus.Error.AlreadyPaired"
    );

    // UnknownObject: a job that is not running is the bus's own refusal, not ours — §4
    // says every `Job1` call may come back this way, and that it is not an error
    // condition of the API.
    let job = Job1Proxy::for_job(&client, &found.id, JobRegistry::FIRST_ID)
        .await
        .unwrap();
    assert_eq!(
        refusal(job.state().await.map(|_| ())).name(),
        "org.freedesktop.DBus.Error.UnknownObject"
    );

    daemon.shutdown().await;
}

/// The next value `property` is announced with, skipping signals that do not mention it.
async fn next_change(
    stream: &mut zbus::fdo::PropertiesChangedStream,
    property: &str,
) -> OwnedValue {
    let deadline = tokio::time::Instant::now() + SIGNAL_TIMEOUT;
    loop {
        let signal = tokio::time::timeout_at(deadline, stream.next())
            .await
            .unwrap_or_else(|_| panic!("no PropertiesChanged for {property} within the timeout"))
            .expect("the signal stream ended");
        let args = signal.args().unwrap();

        if let Some(value) = args.changed_properties.get(property) {
            return OwnedValue::try_from(value.try_clone().unwrap()).unwrap();
        }
    }
}

async fn next_string(stream: &mut zbus::fdo::PropertiesChangedStream, property: &str) -> String {
    String::try_from(next_change(stream, property).await)
        .unwrap_or_else(|_| panic!("{property} did not arrive as a string"))
}

async fn next_u32(stream: &mut zbus::fdo::PropertiesChangedStream, property: &str) -> u32 {
    u32::try_from(next_change(stream, property).await)
        .unwrap_or_else(|_| panic!("{property} did not arrive as a u32"))
}
