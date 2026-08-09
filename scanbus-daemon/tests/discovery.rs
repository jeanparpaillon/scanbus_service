//! Discovery as a session, on a real bus.
//!
//! The acceptance criteria of 2.2, run rather than described: two backends finding one
//! device produce one object, `StopDiscovery` takes away exactly the unpaired ones, a
//! broken backend does not hide a working one, and `GetProfileTypes` answers with what
//! this iteration can actually run.
//!
//! Everything a client learns here arrives through `ObjectManager`, which is the point
//! of §2 having no `ScannerFound` signal: the tests subscribe once, before the first
//! call, and never poll.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use scanbus_core::backend::mock::MockBackend;
use scanbus_core::{
    BackendError, Capabilities, ScannerBackend, ScannerId, ScannerInfo, Status, Value,
};
use scanbus_daemon::dbus::{self, BUS_NAME, Manager1, ObjectRegistry, path};
use scanbus_daemon::{Backends, Discovery, ScannerRegistry};
use zbus::fdo::{ManagedObjects, ObjectManagerProxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value as ZValue};

mod common;

use common::{PrivateBus, skipped};

/// How long a signal that should already be on its way is waited for.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(5);

/// The `Manager1` methods of §2, from the client side.
///
/// Written out here rather than reused from a client crate because there is none yet —
/// 8.1 is what turns these into `scanbus-client` proxies.
#[zbus::proxy(
    interface = "org.scanbus.Manager1",
    default_service = "org.scanbus",
    default_path = "/org/scanbus"
)]
trait Manager {
    fn start_discovery(&self, filters: HashMap<String, OwnedValue>) -> zbus::Result<()>;
    fn stop_discovery(&self) -> zbus::Result<()>;
    fn get_profile_types(&self) -> zbus::Result<Vec<String>>;
}

/// The `Scanner1` properties 2.2 publishes; the rest are 2.3's.
#[zbus::proxy(interface = "org.scanbus.Scanner1", default_service = "org.scanbus")]
trait Scanner {
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn backend(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn address(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn paired(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn status(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn capabilities(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
    #[zbus(property)]
    fn supported_profiles(&self) -> zbus::Result<Vec<String>>;
}

/// The service side: what `main.rs` wires up, with backends a test chose.
struct Daemon {
    connection: zbus::Connection,
    scanners: Arc<ScannerRegistry>,
    discovery: Arc<Discovery>,
    objects: Arc<ObjectRegistry>,
}

impl Daemon {
    /// Brings the daemon up on `bus`, in the order `main.rs` uses: objects, then name.
    async fn start(bus: &PrivateBus, backends: Backends) -> Self {
        let connection = bus.connect().await;
        let objects = Arc::new(ObjectRegistry::new(connection.clone()).await.unwrap());
        let scanners = Arc::new(ScannerRegistry::new(Arc::clone(&objects)));
        let discovery = Arc::new(Discovery::new(backends, Arc::clone(&scanners)));

        objects
            .add(path::manager(), Manager1::new(Arc::clone(&discovery)))
            .await
            .unwrap();
        dbus::request_name(&connection).await.unwrap();

        Self {
            connection,
            scanners,
            discovery,
            objects,
        }
    }
}

fn scanner_info(backend: &str, address: &str, name: &str) -> ScannerInfo {
    ScannerInfo {
        id: ScannerId::from_backend(backend, address).unwrap(),
        name: name.to_owned(),
        backend: backend.to_owned(),
        address: address.to_owned(),
        capabilities: Capabilities::default(),
        status: Status::Online,
    }
}

fn backend(id: &'static str, scanners: impl IntoIterator<Item = ScannerInfo>) -> Arc<MockBackend> {
    Arc::new(MockBackend::with_scanners(scanners).with_id(id))
}

fn backends(entries: impl IntoIterator<Item = Arc<MockBackend>>) -> Backends {
    Backends::new(
        entries
            .into_iter()
            .map(|backend| backend as Arc<dyn ScannerBackend>),
    )
}

async fn manager_proxy(connection: &zbus::Connection) -> ObjectManagerProxy<'static> {
    ObjectManagerProxy::builder(connection)
        .destination(BUS_NAME)
        .unwrap()
        .path(path::ROOT)
        .unwrap()
        .build()
        .await
        .unwrap()
}

async fn scanner_proxy(connection: &zbus::Connection, id: &ScannerId) -> ScannerProxy<'static> {
    ScannerProxy::builder(connection)
        .path(path::scanner(id))
        .unwrap()
        .build()
        .await
        .unwrap()
}

/// The scanner objects `GetManagedObjects` reports, in path order.
///
/// The structural path elements zbus lists with an empty interface map are dropped here;
/// `object_tree.rs` is where that behaviour is pinned down.
fn scanner_paths(managed: &ManagedObjects) -> Vec<String> {
    let mut paths: Vec<String> = managed
        .iter()
        .filter(|(_, interfaces)| interfaces.contains_key("org.scanbus.Scanner1"))
        .map(|(path, _)| path.as_str().to_owned())
        .collect();
    paths.sort();
    paths
}

/// The path of the next `InterfacesAdded` naming a `Scanner1`.
async fn next_scanner_added(
    stream: &mut zbus::fdo::InterfacesAddedStream,
) -> (OwnedObjectPath, HashMap<String, OwnedValue>) {
    loop {
        let signal = tokio::time::timeout(SIGNAL_TIMEOUT, stream.next())
            .await
            .expect("no InterfacesAdded within the timeout")
            .unwrap();
        let args = signal.args().unwrap();

        if let Some(properties) = args.interfaces_and_properties.get("org.scanbus.Scanner1") {
            let properties = properties
                .iter()
                .map(|(name, value)| {
                    (
                        (*name).to_owned(),
                        OwnedValue::try_from(value.try_clone().unwrap()).unwrap(),
                    )
                })
                .collect();
            return (args.object_path().to_owned().into(), properties);
        }
    }
}

/// The path of the next `InterfacesRemoved` naming a `Scanner1`.
async fn next_scanner_removed(stream: &mut zbus::fdo::InterfacesRemovedStream) -> OwnedObjectPath {
    loop {
        let signal = tokio::time::timeout(SIGNAL_TIMEOUT, stream.next())
            .await
            .expect("no InterfacesRemoved within the timeout")
            .unwrap();
        let args = signal.args().unwrap();

        if args
            .interfaces
            .iter()
            .any(|name| *name == "org.scanbus.Scanner1")
        {
            return args.object_path().to_owned().into();
        }
    }
}

/// Acceptance: one device, two backends, one object — and `Backend` names the winner.
///
/// Run in both registration orders, because "one object appears" on its own would also
/// hold if the second backend were never probed: what has to be shown is that the *list
/// order* is what decides, which is the rule [`scanbus_daemon::backends`] writes down.
#[tokio::test]
async fn two_backends_finding_one_device_publish_one_scanner() {
    // The same eSCL device, spelled the way each backend spells it (§9).
    let escl_sighting = scanner_info(
        "escl",
        "http://192.168.1.50:80/eSCL/",
        "Brother MFC-L2710DW",
    );
    let sane_sighting = scanner_info(
        "sane",
        "airscan:escl:http://192.168.1.50:80/eSCL/",
        "MFC-L2710DW series",
    );

    for (first, second) in [("escl", "sane"), ("sane", "escl")] {
        // A bus per iteration: the second daemon must be able to own the name without
        // waiting for the first one's disconnect to be processed.
        let Some(bus) = PrivateBus::start().await else {
            return skipped("two_backends_finding_one_device_publish_one_scanner");
        };

        let sightings = |id: &str| match id {
            "escl" => escl_sighting.clone(),
            _ => sane_sighting.clone(),
        };
        let daemon = Daemon::start(
            &bus,
            backends([
                backend(first, [sightings(first)]),
                backend(second, [sightings(second)]),
            ]),
        )
        .await;

        let client = bus.connect().await;
        let manager = manager_proxy(&client).await;
        let mut added = manager.receive_interfaces_added().await.unwrap();

        ManagerProxy::new(&client)
            .await
            .unwrap()
            .start_discovery(HashMap::new())
            .await
            .unwrap();

        let (path, properties) = next_scanner_added(&mut added).await;
        assert_eq!(
            String::try_from(properties["Backend"].clone()).unwrap(),
            first,
            "the backend registered first should own the object"
        );
        assert!(!bool::try_from(properties["Paired"].clone()).unwrap());

        // And the loser produced no second object.
        assert_eq!(
            scanner_paths(&manager.get_managed_objects().await.unwrap()),
            vec![path.as_str().to_owned()]
        );
        assert_eq!(daemon.scanners.ids().await.len(), 1);

        daemon.discovery.stop().await;
        daemon.objects.shutdown().await;
    }
}

/// Acceptance: `InterfacesAdded` per scanner, `InterfacesRemoved` for exactly the
/// unpaired ones — the lifetime rule of §1, seen from a client.
#[tokio::test]
async fn stopping_discovery_removes_the_unpaired_and_keeps_the_paired() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("stopping_discovery_removes_the_unpaired_and_keeps_the_paired");
    };

    let found = [
        scanner_info("escl", "http://192.168.1.50:80/eSCL/", "an eSCL scanner"),
        scanner_info("escl", "http://192.168.1.51:80/eSCL/", "another one"),
    ];
    let daemon = Daemon::start(&bus, backends([backend("escl", found.clone())])).await;

    // A paired scanner no backend reports at all: the acceptance criterion that it
    // survives a whole StartDiscovery/StopDiscovery cycle.
    let paired = scanner_info("escl", "usb:001:002", "a paired scanner");
    daemon
        .scanners
        .register_persistent(paired.clone())
        .await
        .unwrap();

    let client = bus.connect().await;
    let manager = manager_proxy(&client).await;
    let mut added = manager.receive_interfaces_added().await.unwrap();
    let mut removed = manager.receive_interfaces_removed().await.unwrap();
    let proxy = ManagerProxy::new(&client).await.unwrap();

    proxy.start_discovery(HashMap::new()).await.unwrap();

    let mut appeared = vec![
        next_scanner_added(&mut added).await.0,
        next_scanner_added(&mut added).await.0,
    ];
    appeared.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    let expected: Vec<OwnedObjectPath> = found.iter().map(|info| path::scanner(&info.id)).collect();
    assert_eq!(appeared, expected);

    proxy.stop_discovery().await.unwrap();

    let mut went = vec![
        next_scanner_removed(&mut removed).await,
        next_scanner_removed(&mut removed).await,
    ];
    went.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    assert_eq!(went, expected, "exactly the unpaired ones should go");

    // The paired one is untouched, and still says so.
    assert_eq!(
        scanner_paths(&manager.get_managed_objects().await.unwrap()),
        vec![path::scanner(&paired.id).as_str().to_owned()]
    );
    assert!(
        scanner_proxy(&client, &paired.id)
            .await
            .paired()
            .await
            .unwrap()
    );
    assert!(!daemon.discovery.is_running().await);
}

/// Acceptance: a backend whose `discover()` errors is logged and skipped, and the other
/// backend's scanners still appear.
#[tokio::test]
async fn a_failing_backend_does_not_hide_the_others() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_failing_backend_does_not_hide_the_others");
    };

    let broken = backend("sane", []);
    broken
        .handle()
        .fail_discovery(BackendError::Other("no scanimage on PATH".to_owned()));
    let working = backend(
        "escl",
        [scanner_info(
            "escl",
            "http://192.168.1.50:80/eSCL/",
            "a scanner",
        )],
    );

    let _daemon = Daemon::start(&bus, backends([broken, working])).await;
    let client = bus.connect().await;
    let manager = manager_proxy(&client).await;
    let mut added = manager.receive_interfaces_added().await.unwrap();

    ManagerProxy::new(&client)
        .await
        .unwrap()
        .start_discovery(HashMap::new())
        .await
        .unwrap();

    let (_, properties) = next_scanner_added(&mut added).await;
    assert_eq!(
        String::try_from(properties["Backend"].clone()).unwrap(),
        "escl"
    );
}

/// Acceptance: `GetProfileTypes` returns what this iteration can run, not §2's four.
#[tokio::test]
async fn get_profile_types_advertises_only_the_implemented_profiles() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("get_profile_types_advertises_only_the_implemented_profiles");
    };

    let _daemon = Daemon::start(&bus, Backends::default()).await;
    let client = bus.connect().await;

    assert_eq!(
        ManagerProxy::new(&client)
            .await
            .unwrap()
            .get_profile_types()
            .await
            .unwrap(),
        ["image", "document"]
    );
}

/// A backend name no backend answers to is a client bug, and is refused before anything
/// starts — unlike a backend that fails to probe, which is the machine's problem.
#[tokio::test]
async fn an_unknown_backend_filter_is_invalid_args() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("an_unknown_backend_filter_is_invalid_args");
    };

    let daemon = Daemon::start(&bus, backends([backend("escl", [])])).await;
    let client = bus.connect().await;
    let proxy = ManagerProxy::new(&client).await.unwrap();

    let filters = HashMap::from([(
        "backends".to_owned(),
        OwnedValue::try_from(ZValue::from(vec!["avahi".to_owned()])).unwrap(),
    )]);
    let error = proxy
        .start_discovery(filters)
        .await
        .expect_err("an unknown backend must be refused");

    match error {
        zbus::Error::MethodError(name, message, _) => {
            assert_eq!(name.as_str(), "org.freedesktop.DBus.Error.InvalidArgs");
            assert!(
                message.as_deref().is_some_and(|m| m.contains("avahi")),
                "the error should name what was asked for: {message:?}"
            );
        }
        other => panic!("expected a named D-Bus error, got {other:?}"),
    }

    // Refused before anything started: §2's filters are checked, not partially applied.
    assert!(!daemon.discovery.is_running().await);

    // And a filter naming a real backend is accepted.
    let filters = HashMap::from([(
        "backends".to_owned(),
        OwnedValue::try_from(ZValue::from(vec!["escl".to_owned()])).unwrap(),
    )]);
    proxy.start_discovery(filters).await.unwrap();
}

/// §2: a second `StartDiscovery` restarts nothing and returns successfully — the
/// objects the first client is watching must not blink.
#[tokio::test]
async fn a_second_start_discovery_restarts_nothing() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_second_start_discovery_restarts_nothing");
    };

    let found = scanner_info("escl", "http://192.168.1.50:80/eSCL/", "a scanner");
    let _daemon = Daemon::start(&bus, backends([backend("escl", [found.clone()])])).await;

    let client = bus.connect().await;
    let manager = manager_proxy(&client).await;
    let mut added = manager.receive_interfaces_added().await.unwrap();
    let mut removed = manager.receive_interfaces_removed().await.unwrap();
    let proxy = ManagerProxy::new(&client).await.unwrap();

    proxy.start_discovery(HashMap::new()).await.unwrap();
    assert_eq!(
        next_scanner_added(&mut added).await.0,
        path::scanner(&found.id)
    );

    proxy.start_discovery(HashMap::new()).await.unwrap();
    proxy.start_discovery(HashMap::new()).await.unwrap();

    // Nothing was removed and nothing was added a second time: the running session was
    // joined, not restarted. A round is 5 s away, so anything arriving here is churn.
    assert!(
        tokio::time::timeout(Duration::from_millis(300), removed.next())
            .await
            .is_err(),
        "restarting the session would have removed the object"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), added.next())
            .await
            .is_err(),
        "the scanner was published twice"
    );
    assert_eq!(
        scanner_paths(&manager.get_managed_objects().await.unwrap()).len(),
        1
    );
}

/// §1: a paired scanner rediscovered updates the object it already has. The client sees
/// `PropertiesChanged`, not a second scanner.
#[tokio::test]
async fn a_rediscovered_paired_scanner_updates_its_object() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_rediscovered_paired_scanner_updates_its_object");
    };

    let paired = scanner_info("escl", "http://192.168.1.50:80/eSCL/", "a paired scanner");

    // The same scanner, as a later probe finds it: renamed on its front panel, busy,
    // and now reporting a capability the daemon knew nothing about.
    let mut rediscovered = paired.clone();
    rediscovered.name = "Reception scanner".to_owned();
    rediscovered.status = Status::Busy;
    rediscovered.capabilities = Capabilities {
        resolutions: vec![300, 600],
        extra: [("firmware".to_owned(), Value::Str("1.42".to_owned()))]
            .into_iter()
            .collect(),
        ..Capabilities::default()
    };

    let daemon = Daemon::start(&bus, backends([backend("escl", [rediscovered.clone()])])).await;
    daemon
        .scanners
        .register_persistent(paired.clone())
        .await
        .unwrap();

    let client = bus.connect().await;
    let manager = manager_proxy(&client).await;
    let mut added = manager.receive_interfaces_added().await.unwrap();
    let scanner = scanner_proxy(&client, &paired.id).await;
    let mut names = scanner.receive_name_changed().await;

    ManagerProxy::new(&client)
        .await
        .unwrap()
        .start_discovery(HashMap::new())
        .await
        .unwrap();

    let changed = tokio::time::timeout(SIGNAL_TIMEOUT, names.next())
        .await
        .expect("no PropertiesChanged for Name within the timeout")
        .unwrap();
    assert_eq!(changed.get().await.unwrap(), "Reception scanner");

    assert_eq!(scanner.status().await.unwrap(), "busy");
    assert!(scanner.paired().await.unwrap(), "the pairing survived");
    assert_eq!(
        Vec::<u32>::try_from(scanner.capabilities().await.unwrap()["resolutions"].clone()).unwrap(),
        [300, 600]
    );

    // No second object, and no `InterfacesAdded` for the one already there.
    assert!(
        tokio::time::timeout(Duration::from_millis(300), added.next())
            .await
            .is_err(),
        "a rediscovery must not publish a second object"
    );
    assert_eq!(daemon.scanners.ids().await, vec![paired.id.clone()]);

    // Stopping the session leaves the paired object alone, however it got there.
    ManagerProxy::new(&client)
        .await
        .unwrap()
        .stop_discovery()
        .await
        .unwrap();
    assert_eq!(
        scanner_paths(&manager.get_managed_objects().await.unwrap()),
        vec![path::scanner(&paired.id).as_str().to_owned()]
    );
}

/// Shutting the daemon down while a session runs must not leave the task publishing
/// into a tree that is being taken apart.
#[tokio::test]
async fn shutdown_stops_the_session_before_the_tree_goes() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("shutdown_stops_the_session_before_the_tree_goes");
    };

    let found = scanner_info("escl", "http://192.168.1.50:80/eSCL/", "a scanner");
    let daemon = Daemon::start(&bus, backends([backend("escl", [found.clone()])])).await;

    let client = bus.connect().await;
    let manager = manager_proxy(&client).await;
    let mut added = manager.receive_interfaces_added().await.unwrap();
    ManagerProxy::new(&client)
        .await
        .unwrap()
        .start_discovery(HashMap::new())
        .await
        .unwrap();
    next_scanner_added(&mut added).await;

    // The order `main.rs` uses.
    daemon.discovery.stop().await;
    daemon.objects.shutdown().await;

    assert!(daemon.objects.is_empty().await);
    assert!(daemon.scanners.ids().await.is_empty());
    drop(daemon.connection);
}
