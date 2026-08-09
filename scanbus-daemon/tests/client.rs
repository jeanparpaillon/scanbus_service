//! `scanbus-client` against a real daemon, on a real bus.
//!
//! The acceptance criteria of 8.1, and the reason the crate is a dev-dependency here
//! rather than a second copy of the proxies in `tests/`: these assertions are about the
//! *client* — that its proxies round-trip against what this daemon actually serves, that
//! its error decoding names what the daemon actually sent, that its watcher does not lose
//! a transition — and none of them mean anything against a mock of the daemon.
//!
//! They live in `scanbus-daemon/tests/` because that is where a daemon exists to answer
//! them. `scanbus-client` has no bus of its own and must not grow one: a client crate
//! that could stand up the service would be a client crate that depends on it.
//!
//! 2.8 grows this into the full conformance suite; what is here is the client's half.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use scanbus_client::proxy::{Manager1Proxy, Scanner1Proxy, object_manager};
use scanbus_client::{
    Bus, Error, Presence, ScanbusError, ScannerState, ScannerWatch, convert, presence,
};
use scanbus_core::backend::mock::MockBackend;
use scanbus_core::{
    ButtonsCapability, Capabilities, ColorMode, PairingState, ProfileKind, ScannerBackend,
    ScannerId, ScannerInfo, Source, Status, Value, path,
};
use scanbus_daemon::Backends;

mod common;

use common::{Daemon, PrivateBus, skipped};

/// How long a signal that should already be on its way is waited for.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(5);

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
            },
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

fn backends(scanners: impl IntoIterator<Item = ScannerInfo>) -> Backends {
    Backends::new([
        Arc::new(MockBackend::with_scanners(scanners).with_id("mock")) as Arc<dyn ScannerBackend>,
    ])
}

/// Starts discovery through the client's own proxy and waits for the object.
async fn discover(client: &zbus::Connection, id: &ScannerId) {
    let manager = object_manager(client).await.unwrap();
    let mut added = manager.receive_interfaces_added().await.unwrap();

    Manager1Proxy::new(client)
        .await
        .unwrap()
        .start_discovery(HashMap::new())
        .await
        .unwrap();

    let wanted = path::scanner(id);
    loop {
        let signal = tokio::time::timeout(SIGNAL_TIMEOUT, added.next())
            .await
            .expect("no InterfacesAdded within the timeout")
            .unwrap();
        if signal.args().unwrap().object_path().as_str() == wanted {
            return;
        }
    }
}

/// Acceptance: each proxy round-trips one method and one property against the daemon.
///
/// `Profile1` still has no server side (3.1), and a `Job1` object only exists while a
/// scan is in flight (§4), so what is asserted for those two is the honest half: their
/// proxies build against the paths of §1 and their calls come back as a refusal the
/// client decodes *by name* — not as a transport failure, which is what a client that
/// guessed would report.
#[tokio::test]
async fn every_proxy_round_trips_against_the_daemon() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("every_proxy_round_trips_against_the_daemon");
    };

    let found = brother();
    let daemon = Daemon::start(&bus, backends([found.clone()])).await;
    let client = bus.connect().await;

    // Manager1: a method, and the ObjectManager §2 uses instead of a signal of its own.
    let manager = Manager1Proxy::new(&client).await.unwrap();
    assert_eq!(
        manager.get_profile_types().await.unwrap(),
        ["image", "document"]
    );
    discover(&client, &found.id).await;
    assert!(
        object_manager(&client)
            .await
            .unwrap()
            .get_managed_objects()
            .await
            .unwrap()
            .contains_key(&*scanbus_daemon::dbus::path::scanner(&found.id))
    );

    // Scanner1: a property, then a method.
    let scanner = Scanner1Proxy::for_scanner(&client, &found.id)
        .await
        .unwrap();
    assert_eq!(scanner.name().await.unwrap(), found.name);
    assert_eq!(scanner.scanner_id(), Some(found.id.clone()));
    scanner.cancel_pairing().await.unwrap();

    // And the property the whole `a{sv}` decoding exists for, including the key this
    // client has no field for.
    let capabilities = convert::capabilities(&scanner.capabilities().await.unwrap()).unwrap();
    assert_eq!(capabilities, found.capabilities);
    assert_eq!(
        capabilities.extra.get("max_scan_area_mm"),
        Some(&Value::Array(vec![Value::U64(216), Value::U64(356)])),
        "a capability key this version does not model must survive the round trip"
    );

    // Button1: published with its scanner (2.5), so this is a plain round trip. Four
    // keys, no host-settable labels — the Brother of §5.
    let button = scanbus_client::proxy::Button1Proxy::for_button(&client, &found.id, 0)
        .await
        .unwrap();
    assert_eq!(button.index().await.unwrap(), 0);
    assert!(!button.label_configurable().await.unwrap());
    assert_eq!(button.profile().await.unwrap(), "");

    // Job1 and Profile1, which have no object at these paths — and for two different
    // reasons worth keeping apart. A job is *transient* (§4): its object exists only
    // while a scan is in flight, so a proxy built for an id nothing is running is
    // `UnknownObject` by design, and `tests/jobs.rs` is where one is driven for real.
    // `Profile1` has no server side at all yet (3.1).
    //
    // Either way what a client gets is a *named refusal* it classifies as one —
    // `Error::Call`, not `Error::Bus`. That distinction is what lets the CLI say "there
    // is no such object" instead of "the bus is broken".
    let job = scanbus_client::proxy::Job1Proxy::for_job(&client, &found.id, 1)
        .await
        .unwrap();
    let profile = scanbus_client::proxy::Profile1Proxy::for_profile(&client, ProfileKind::Document)
        .await
        .unwrap();

    let refusals = [
        job.state().await.map(|_| ()),
        profile.options().await.map(|_| ()),
    ];
    for refusal in refusals {
        let error = Error::from(refusal.expect_err("there is no object at that path"));
        match &error {
            Error::Call(ScanbusError::Other { name, .. }) => assert_eq!(
                name, "org.freedesktop.DBus.Error.UnknownObject",
                "an object that is not there is what the bus reports, by name"
            ),
            other => panic!("expected a named refusal, got {other:?}"),
        }
    }

    daemon.shutdown().await;
}

/// Acceptance: `ScannerState` reads the whole of §3 off a live object.
#[tokio::test]
async fn a_get_all_reply_from_the_daemon_decodes_into_the_model() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_get_all_reply_from_the_daemon_decodes_into_the_model");
    };

    let found = brother();
    let daemon = Daemon::start(&bus, backends([found.clone()])).await;
    let client = bus.connect().await;
    discover(&client, &found.id).await;

    let properties = scanbus_client::proxy::properties(&client, path::scanner(&found.id))
        .await
        .unwrap()
        .get_all("org.scanbus.Scanner1".try_into().unwrap())
        .await
        .unwrap();
    let state = ScannerState::from_properties(&properties).unwrap();

    assert_eq!(state.id, found.id);
    assert_eq!(state.name, found.name);
    assert_eq!(state.backend, found.backend);
    assert_eq!(state.address, found.address);
    assert_eq!(state.capabilities, found.capabilities);
    assert_eq!(state.status, Status::Online);
    assert_eq!(
        state.supported_profiles,
        [ProfileKind::Image, ProfileKind::Document]
    );
    assert!(!state.paired);
    assert!(!state.connected);
    assert_eq!(state.default_profile, None);
    assert_eq!(state.pairing, PairingState::None);

    daemon.shutdown().await;
}

/// Acceptance: `org.scanbus.Error.NotPaired` decodes to the variant, and an unknown name
/// to `Other` with the name intact rather than to a failure.
///
/// The first half is the one that cannot be unit-tested: an `org.scanbus.Error.*` reaches
/// a client as a [`zbus::Error::MethodError`] carrying a whole message, which only a bus
/// produces.
#[tokio::test]
async fn a_named_refusal_decodes_to_its_variant() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_named_refusal_decodes_to_its_variant");
    };

    let found = brother();
    let daemon = Daemon::start(&bus, backends([found.clone()])).await;
    let client = bus.connect().await;
    discover(&client, &found.id).await;

    let scanner = Scanner1Proxy::for_scanner(&client, &found.id)
        .await
        .unwrap();

    // §8's name, from a daemon that means it: unpairing a scanner that was never paired.
    let error = Error::from(
        scanner
            .unpair()
            .await
            .expect_err("a scanner that was never paired cannot be unpaired"),
    );
    match &error {
        Error::Call(call @ ScanbusError::NotPaired(detail)) => {
            assert!(detail.contains(found.id.as_str()), "{detail}");
            assert_eq!(call.name(), "org.scanbus.Error.NotPaired");
            assert_eq!(call.exit_code(), 7);
        }
        other => panic!("expected NotPaired, got {other:?}"),
    }

    // A standard name is named too, and must not be mistaken for a transport failure:
    // §3 documents no `Pair()` option, so one is `InvalidArgs`.
    let options = HashMap::from([(
        "pin".to_owned(),
        zbus::zvariant::OwnedValue::try_from(zbus::zvariant::Value::from("123456")).unwrap(),
    )]);
    match Error::from(scanner.pair(options).await.expect_err("an unknown option")) {
        Error::Call(ScanbusError::Other { name, message }) => {
            assert_eq!(name, "org.freedesktop.DBus.Error.InvalidArgs");
            assert!(message.contains("pin"), "{message}");
        }
        other => panic!("expected a named InvalidArgs, got {other:?}"),
    }

    // And a name this client has never heard of keeps its name rather than becoming
    // "some error" — the case a daemon newer than the client produces.
    let unknown = ScanbusError::from_reply("org.scanbus.Error.Xyz", "the lid is open");
    assert_eq!(unknown.name(), "org.scanbus.Error.Xyz");
    assert_eq!(unknown.exit_code(), 1);

    daemon.shutdown().await;
}

/// Acceptance: a transition that happened between the call and the first poll of the
/// stream is reported, not lost.
///
/// The mock backend pairs instantly, so by the time `Pair()` has returned and the
/// property says `done` the whole `pairing → installing_backend → done` sequence has
/// already been emitted — before anything read the stream. A watcher that took its
/// snapshot and then *resubscribed*, or that dropped what was queued without reading the
/// snapshot, would report `none` and wait out its timeout on a scanner that is paired.
#[tokio::test]
async fn a_transition_between_the_call_and_the_first_poll_is_not_lost() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_transition_between_the_call_and_the_first_poll_is_not_lost");
    };

    let found = brother();
    let daemon = Daemon::start(&bus, backends([found.clone()])).await;
    let client = bus.connect().await;
    discover(&client, &found.id).await;

    let scanner = Scanner1Proxy::for_scanner(&client, &found.id)
        .await
        .unwrap();
    assert_eq!(scanner.pairing_state().await.unwrap(), "none");

    // 1. subscribe, 2. call — the two steps §7 puts in this order.
    let watch = ScannerWatch::subscribe(&client, path::scanner(&found.id))
        .await
        .unwrap();
    scanner.pair(HashMap::new()).await.unwrap();

    // The pairing runs to completion here, with nothing consuming the signal stream.
    tokio::time::timeout(SIGNAL_TIMEOUT, async {
        while scanner.pairing_state().await.unwrap() != "done" {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the mock backend should pair well within the timeout");

    // 3. snapshot, 4. stream. The verdict has to be in the first item: there is no
    // further transition coming, so a watcher that only streams hangs here.
    let mut states = watch.states().await.unwrap();
    let first = tokio::time::timeout(SIGNAL_TIMEOUT, states.next())
        .await
        .expect("the snapshot must arrive without waiting for another signal")
        .expect("the stream ended before yielding the snapshot")
        .unwrap();

    assert_eq!(first.pairing, PairingState::Done);
    assert!(first.paired);

    // And the stale signals queued before the snapshot are gone rather than replayed:
    // a client told `done` must not then be told `pairing`.
    let stale = tokio::time::timeout(Duration::from_millis(200), states.next()).await;
    assert!(
        stale.is_err(),
        "a state was replayed after the snapshot: {stale:?}"
    );

    daemon.shutdown().await;
}

/// The other half of the same criterion: a scanner that changes *after* the snapshot is
/// still followed, so dropping the queued signals did not cost the stream its job.
#[tokio::test]
async fn a_change_after_the_snapshot_is_still_reported() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_change_after_the_snapshot_is_still_reported");
    };

    let found = brother();
    let backend = Arc::new(MockBackend::with_scanners([found.clone()]).with_id("mock"));
    let handle = backend.handle();
    let daemon = Daemon::start(&bus, Backends::new([backend as Arc<dyn ScannerBackend>])).await;
    let client = bus.connect().await;
    discover(&client, &found.id).await;

    let scanner = Scanner1Proxy::for_scanner(&client, &found.id)
        .await
        .unwrap();
    let watch = ScannerWatch::subscribe(&client, path::scanner(&found.id))
        .await
        .unwrap();
    scanner.pair(HashMap::new()).await.unwrap();

    let mut states = watch.states().await.unwrap();
    let first = states.next().await.unwrap().unwrap();
    assert_eq!(first.status, Status::Online);

    // Whatever the snapshot caught, the pairing ends at `done` and the stream says so.
    let mut current = first;
    while current.pairing != PairingState::Done {
        current = tokio::time::timeout(SIGNAL_TIMEOUT, states.next())
            .await
            .expect("no pairing transition within the timeout")
            .expect("the stream ended before the pairing finished")
            .unwrap();
    }
    assert!(current.paired);

    // The scanner is switched off: the next probe round moves `Status`, and the watcher
    // reports a whole state rather than the one property that moved.
    handle.set_discovery([]);
    let offline = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let state = states.next().await.unwrap().unwrap();
            if state.status == Status::Offline {
                return state;
            }
        }
    })
    .await
    .expect("no Status change within a probe round");

    assert!(offline.paired, "an unreachable scanner keeps its pairing");
    assert_eq!(offline.pairing, PairingState::Done);
    assert_eq!(offline.name, found.name);

    daemon.shutdown().await;
}

/// Acceptance: `--no-activate` asks the bus about the name instead of making a call.
///
/// On the private bus nothing is activatable, so a daemon that is not running is
/// `Absent` — and `connect(.., false)` refuses rather than starting anything. That the
/// answer is read from `NameHasOwner` is what makes it side-effect-free; the assertion
/// that it is *correct* both before and after the daemon appears is what shows it is
/// not just returning a constant.
#[tokio::test]
async fn no_activate_refuses_before_the_daemon_owns_the_name() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("no_activate_refuses_before_the_daemon_owns_the_name");
    };

    let address = Bus::Address(bus.address().to_owned());

    let probe = address.connect().await.unwrap();
    assert_eq!(presence(&probe).await.unwrap(), Presence::Absent);
    assert!(matches!(
        scanbus_client::connect(&address, false).await,
        Err(Error::NotRunning)
    ));
    // Activation is allowed to fail on a bus with no service files; what must not happen
    // is a hang, and the connection itself always succeeds.
    assert!(scanbus_client::connect(&address, true).await.is_ok());

    let daemon = Daemon::start(&bus, backends([])).await;
    assert_eq!(presence(&probe).await.unwrap(), Presence::Running);
    assert!(scanbus_client::connect(&address, false).await.is_ok());

    daemon.shutdown().await;
}
