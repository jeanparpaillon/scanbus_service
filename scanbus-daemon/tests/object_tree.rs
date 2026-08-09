//! The object tree, on a real bus.
//!
//! These are the acceptance criteria of 2.1, run rather than described: the manager
//! answers `GetManagedObjects` with an empty dict on a fresh start, introspection lists
//! `ObjectManager` beside the standard interfaces, a second owner of the name is
//! refused instead of queued, and the registry is what makes objects appear and
//! disappear. 2.8 grows this into the full conformance suite.

use std::time::Duration;

use futures_util::StreamExt as _;
use scanbus_core::ScannerId;
use scanbus_daemon::dbus::{self, BUS_NAME, ObjectRegistry, path};
use zbus::fdo::{DBusProxy, IntrospectableProxy, ManagedObjects, ObjectManagerProxy};
use zbus::zvariant::OwnedObjectPath;

mod common;

use common::{PrivateBus, skipped};

/// How long a signal that should already be on its way is waited for.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(5);

/// A stand-in for `Scanner1`, which is 2.3's.
///
/// Only its name and the fact that it has a property matter here: the registry is
/// generic over [`zbus::object_server::Interface`], so what is exported is exactly what
/// `GetManagedObjects` has to report back.
struct Probe;

#[zbus::interface(name = "org.scanbus.test.Probe1")]
impl Probe {
    #[zbus(property)]
    async fn marker(&self) -> u32 {
        42
    }
}

fn scanner_id() -> ScannerId {
    ScannerId::from_backend("sane", "epson2:net:192.168.1.50").unwrap()
}

/// The managed objects that actually carry an interface, in tree order.
///
/// zbus reports every node under the manager, the bare path elements included — see
/// `dbus::objects`. Those are not objects in the sense of §1, so they are filtered out
/// everywhere except in the one assertion that pins the behaviour down.
fn objects_carrying_interfaces(managed: &ManagedObjects) -> Vec<OwnedObjectPath> {
    let mut paths: Vec<OwnedObjectPath> = managed
        .iter()
        .filter(|(_, interfaces)| !interfaces.is_empty())
        .map(|(path, _)| path.clone())
        .collect();
    paths.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    paths
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

/// A fresh daemon: the manager is there, it answers, and nothing is under it.
#[tokio::test]
async fn a_fresh_daemon_serves_an_empty_object_manager() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_fresh_daemon_serves_an_empty_object_manager");
    };

    let service = bus.connect().await;
    let registry = ObjectRegistry::new(service.clone()).await.unwrap();
    dbus::request_name(&service).await.unwrap();

    let client = bus.connect().await;

    // `busctl --user call … GetManagedObjects` returning an empty dict rather than an
    // error: the manager exists before the name does.
    let managed = manager_proxy(&client).await.get_managed_objects().await;
    assert_eq!(managed.unwrap().len(), 0, "nothing should be exported yet");

    // `gdbus introspect …`: ObjectManager alongside the standard interfaces.
    let xml = IntrospectableProxy::builder(&client)
        .destination(BUS_NAME)
        .unwrap()
        .path(path::ROOT)
        .unwrap()
        .build()
        .await
        .unwrap()
        .introspect()
        .await
        .unwrap();

    for interface in [
        "org.freedesktop.DBus.ObjectManager",
        "org.freedesktop.DBus.Introspectable",
        "org.freedesktop.DBus.Peer",
        "org.freedesktop.DBus.Properties",
    ] {
        assert!(xml.contains(interface), "{interface} missing from {xml}");
    }

    // The registry's own view: the manager, and only the manager.
    assert_eq!(registry.paths().await.len(), 1);
    assert!(
        registry
            .interfaces(&path::manager())
            .await
            .is_some_and(|interfaces| interfaces
                .iter()
                .any(|name| name == "org.freedesktop.DBus.ObjectManager"))
    );
}

/// A second instance loses the race and is told so, rather than being queued for a name
/// it would never answer on.
#[tokio::test]
async fn a_second_instance_is_refused_not_queued() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_second_instance_is_refused_not_queued");
    };

    let first = bus.connect().await;
    let _registry = ObjectRegistry::new(first.clone()).await.unwrap();
    dbus::request_name(&first).await.unwrap();

    let second = bus.connect().await;
    let _second_registry = ObjectRegistry::new(second.clone()).await.unwrap();
    let error = dbus::request_name(&second)
        .await
        .expect_err("the second instance must not get the name");
    assert!(
        matches!(error, scanbus_daemon::Error::NameTaken { name } if name == BUS_NAME),
        "expected NameTaken, got {error:?}"
    );

    // And the first instance still owns it — `AllowReplacement` is deliberately not
    // among the flags, so the loser cannot take the name from a working daemon.
    let owner = DBusProxy::new(&first)
        .await
        .unwrap()
        .get_name_owner(BUS_NAME.try_into().unwrap())
        .await
        .unwrap();
    assert_eq!(owner.as_str(), first.unique_name().unwrap().as_str());
}

/// Adding to the registry is what publishes an object, and removing it is what retracts
/// it — the property that lets a client subscribe once and never poll.
#[tokio::test]
async fn adding_and_removing_reach_object_manager_clients() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("adding_and_removing_reach_object_manager_clients");
    };

    let service = bus.connect().await;
    let registry = ObjectRegistry::new(service.clone()).await.unwrap();
    dbus::request_name(&service).await.unwrap();

    let client = bus.connect().await;
    let manager = manager_proxy(&client).await;
    // Subscribed before anything is added, so the signals cannot be missed.
    let mut added = manager.receive_interfaces_added().await.unwrap();
    let mut removed = manager.receive_interfaces_removed().await.unwrap();

    let id = scanner_id();
    let scanner_path = path::scanner(&id);
    registry.add(scanner_path.clone(), Probe).await.unwrap();
    registry.add(path::button(&id, 0), Probe).await.unwrap();

    for expected in [scanner_path.clone(), path::button(&id, 0)] {
        let signal = tokio::time::timeout(SIGNAL_TIMEOUT, added.next())
            .await
            .expect("no InterfacesAdded within the timeout")
            .unwrap();
        let args = signal.args().unwrap();
        assert_eq!(args.object_path(), &*expected);
        assert!(
            args.interfaces_and_properties
                .contains_key("org.scanbus.test.Probe1")
        );
    }

    let managed = manager.get_managed_objects().await.unwrap();
    assert_eq!(
        objects_carrying_interfaces(&managed),
        vec![scanner_path.clone(), path::button(&id, 0)]
    );

    // The structural elements of the path — `…/scanner`, `…/{id}/button` — come back as
    // managed objects with no interface at all. Documented in `dbus::objects`, and
    // asserted here because a client iterating the dict has to tolerate them.
    let structural = OwnedObjectPath::try_from("/org/scanbus/scanner").unwrap();
    assert!(
        managed
            .get(&structural)
            .is_some_and(|interfaces| interfaces.is_empty()),
        "expected {structural} to be listed with no interfaces: {managed:?}"
    );

    // Removing the scanner takes its buttons with it, deepest first.
    registry.remove_subtree(&scanner_path).await.unwrap();

    for expected in [path::button(&id, 0), scanner_path.clone()] {
        let signal = tokio::time::timeout(SIGNAL_TIMEOUT, removed.next())
            .await
            .expect("no InterfacesRemoved within the timeout")
            .unwrap();
        assert_eq!(signal.args().unwrap().object_path(), &*expected);
    }

    assert!(
        objects_carrying_interfaces(&manager.get_managed_objects().await.unwrap()).is_empty(),
        "an object survived its removal"
    );
    assert_eq!(registry.paths().await, vec![path::manager().into_inner()]);
}

/// The prefix trap: `…/scanner/x` and `…/scanner/xy` are siblings, not parent and child.
#[tokio::test]
async fn removing_a_subtree_spares_a_sibling_with_a_shared_prefix() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("removing_a_subtree_spares_a_sibling_with_a_shared_prefix");
    };

    let service = bus.connect().await;
    let registry = ObjectRegistry::new(service.clone()).await.unwrap();

    let short = ScannerId::new("sane_usb").unwrap();
    let long = ScannerId::new("sane_usb2").unwrap();
    registry.add(path::scanner(&short), Probe).await.unwrap();
    registry.add(path::button(&short, 0), Probe).await.unwrap();
    registry.add(path::scanner(&long), Probe).await.unwrap();

    registry
        .remove_subtree(&path::scanner(&short))
        .await
        .unwrap();

    assert_eq!(
        registry.paths().await,
        vec![
            path::manager().into_inner(),
            path::scanner(&long).into_inner()
        ]
    );
}

/// Shutdown leaves nothing behind, so a restart never finds a stale path.
#[tokio::test]
async fn shutdown_unexports_everything_including_the_manager() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("shutdown_unexports_everything_including_the_manager");
    };

    let service = bus.connect().await;
    let registry = ObjectRegistry::new(service.clone()).await.unwrap();
    dbus::request_name(&service).await.unwrap();

    let id = scanner_id();
    registry.add(path::scanner(&id), Probe).await.unwrap();
    registry.add(path::button(&id, 0), Probe).await.unwrap();
    registry.add(path::job(&id, 1), Probe).await.unwrap();
    assert_eq!(registry.len().await, 4);

    registry.shutdown().await;
    assert!(registry.is_empty().await);

    // Idempotent: the shutdown path may run twice on a signal race.
    registry.shutdown().await;

    // Nothing is left to introspect: zbus drops a node once its last interface goes, so
    // `/org/scanbus` itself is gone rather than lingering as an empty object a client
    // could still discover.
    let client = bus.connect().await;
    let error = IntrospectableProxy::builder(&client)
        .destination(BUS_NAME)
        .unwrap()
        .path(path::ROOT)
        .unwrap()
        .build()
        .await
        .unwrap()
        .introspect()
        .await
        .expect_err("the object tree survived shutdown");
    assert!(
        matches!(error, zbus::fdo::Error::UnknownObject(_)),
        "expected UnknownObject, got {error:?}"
    );
}

/// Removing a scanner while its buttons are exported would take them off the bus with
/// no `InterfacesRemoved`, leaving the registry tracking objects nobody serves. Refused.
#[tokio::test]
async fn removing_an_object_with_descendants_is_refused() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("removing_an_object_with_descendants_is_refused");
    };

    let service = bus.connect().await;
    let registry = ObjectRegistry::new(service.clone()).await.unwrap();

    let id = scanner_id();
    let scanner_path = path::scanner(&id);
    registry.add(scanner_path.clone(), Probe).await.unwrap();
    registry.add(path::button(&id, 0), Probe).await.unwrap();

    let error = registry
        .remove_object(&scanner_path)
        .await
        .expect_err("removing a scanner with buttons must fail");
    assert!(
        matches!(
            error,
            scanbus_daemon::Error::HasDescendants { descendants: 1, .. }
        ),
        "expected HasDescendants, got {error:?}"
    );

    // Nothing was removed on the way to refusing.
    assert_eq!(registry.len().await, 3);

    // The button first, then the scanner, is what the caller should have done.
    registry.remove_object(&path::button(&id, 0)).await.unwrap();
    registry.remove_object(&scanner_path).await.unwrap();
    assert_eq!(registry.paths().await, vec![path::manager().into_inner()]);
}

/// Exporting the same interface twice is a bug in the caller, and says so.
#[tokio::test]
async fn a_double_export_is_refused() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_double_export_is_refused");
    };

    let service = bus.connect().await;
    let registry = ObjectRegistry::new(service.clone()).await.unwrap();

    let path = path::scanner(&scanner_id());
    registry.add(path.clone(), Probe).await.unwrap();
    let error = registry
        .add(path.clone(), Probe)
        .await
        .expect_err("the second export must fail");
    assert!(
        matches!(error, scanbus_daemon::Error::AlreadyExported { .. }),
        "expected AlreadyExported, got {error:?}"
    );

    // And the failure did not corrupt the tracking: one object, removable.
    assert_eq!(registry.len().await, 2);
    registry.remove_object(&path).await.unwrap();
    let error = registry
        .remove_object(&path)
        .await
        .expect_err("removing it twice must fail");
    assert!(
        matches!(error, scanbus_daemon::Error::NotExported { .. }),
        "expected NotExported, got {error:?}"
    );
}
