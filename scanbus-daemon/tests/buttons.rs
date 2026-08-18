//! `Button1`, on a real bus: the acceptance criteria of 2.5, run rather than described.
//!
//! The object tree is asserted through `GetManagedObjects` rather than by reading the
//! registry, because "visible in `busctl --user tree org.scanbus`" is the criterion and
//! the manager's dict is what `busctl` walks. Entries with no interfaces are the
//! structural path elements `…/scanner` and `…/{id}/button` (see
//! [`ObjectRegistry`](scanbus_daemon::dbus::ObjectRegistry)); nothing here counts them.
//!
//! Nothing sleeps: the failing-write case is driven by
//! [`MockHandle::fail_button_mapping`], which is a statement a test makes between two
//! `await`s rather than a read-only `/opt` it has to arrange.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use scanbus_core::backend::mock::{MockBackend, MockCall, MockHandle};
use scanbus_core::{
    BackendError, ButtonInfo, ButtonsCapability, Capabilities, PairingStore, ProfileKind,
    ScannerBackend, ScannerId, ScannerInfo, Status,
};
use scanbus_daemon::backends::RankedBackend;
use scanbus_daemon::dbus::{self, BUS_NAME, Manager1, ObjectRegistry, Profile1, path};
use scanbus_daemon::{
    Backends, Discovery, JsonPairingStore, MemoryPairingStore, ProfileRegistry, ScannerRegistry,
};
use tempfile::TempDir;
use zbus::fdo::{ObjectManagerProxy, PropertiesChangedStream, PropertiesProxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value as ZValue};

mod common;

use common::{PrivateBus, skipped};

/// How long a signal that should already be on its way is waited for.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(5);

/// What the startup restore gives one scanner's backend before it moves on — `main.rs`'s
/// `RESTORE_TIMEOUT`. Nothing here reaches it: the mock answers immediately.
const RESTORE_TIMEOUT: Duration = Duration::from_secs(5);

/// `org.scanbus.Button1` as §5 defines it, from the client side.
///
/// Property caching is off where this proxy is built ([`button_proxy`]): every assertion
/// is about what the daemon serves *now*, and a cached read would be asserting on how
/// fast the proxy processed a signal.
#[zbus::proxy(interface = "org.scanbus.Button1", default_service = "org.scanbus")]
trait Button {
    #[zbus(property)]
    fn index(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn device_label(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn label_configurable(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn label(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_label(&self, value: &str) -> zbus::Result<()>;
    #[zbus(property)]
    fn profile(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_profile(&self, value: &str) -> zbus::Result<()>;
    #[zbus(property)]
    fn profile_options(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
    #[zbus(property)]
    fn set_profile_options(&self, value: HashMap<String, OwnedValue>) -> zbus::Result<()>;
}

/// Just enough of `org.scanbus.Scanner1` for the pairing and the capability read.
#[zbus::proxy(interface = "org.scanbus.Scanner1", default_service = "org.scanbus")]
trait Scanner {
    fn pair(&self, options: HashMap<String, OwnedValue>) -> zbus::Result<()>;
    fn unpair(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn paired(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn capabilities(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
}

/// The service side: what `main.rs` wires up, with a backend a test drives.
struct Daemon {
    scanners: Arc<ScannerRegistry>,
    discovery: Arc<Discovery>,
    objects: Arc<ObjectRegistry>,
    backend: Arc<MockBackend>,
}

impl Daemon {
    async fn start(bus: &PrivateBus, scanners: impl IntoIterator<Item = ScannerInfo>) -> Self {
        let (daemon, connection) =
            Self::wire(bus, Arc::new(MemoryPairingStore::new()), scanners).await;
        dbus::request_name(&connection).await.unwrap();
        daemon
    }

    /// `main.rs`'s own startup order, with `store` behind it: objects exported, then the
    /// restore of 4.2, and only then the name.
    ///
    /// The order is the point rather than a detail of the harness — a client that
    /// resolves `org.scanbus` the instant it appears has to find every persisted
    /// assignment already on its key and already on the device (5.11), which it cannot if
    /// the replay is something that happens after the name is up.
    async fn start_restoring(
        bus: &PrivateBus,
        store: Arc<dyn PairingStore>,
        scanners: impl IntoIterator<Item = ScannerInfo>,
    ) -> Self {
        Self::start_restoring_with(bus, store, scanners, |_| {}).await
    }

    /// [`start_restoring`](Self::start_restoring) with `arm` run against the backend
    /// between the wiring and the restore — the only moment at which a test can make the
    /// device refuse *during* startup rather than after it.
    async fn start_restoring_with(
        bus: &PrivateBus,
        store: Arc<dyn PairingStore>,
        scanners: impl IntoIterator<Item = ScannerInfo>,
        arm: impl FnOnce(&MockHandle),
    ) -> Self {
        let (daemon, connection) = Self::wire(bus, store, scanners).await;
        arm(&daemon.handle());
        daemon
            .scanners
            .restore(
                &Backends::new([Arc::clone(&daemon.backend) as Arc<dyn ScannerBackend>]),
                RESTORE_TIMEOUT,
            )
            .await;
        dbus::request_name(&connection).await.unwrap();
        daemon
    }

    /// Everything `main.rs` does before it takes the name, and nothing after it.
    async fn wire(
        bus: &PrivateBus,
        store: Arc<dyn PairingStore>,
        scanners: impl IntoIterator<Item = ScannerInfo>,
    ) -> (Self, zbus::Connection) {
        let connection = bus.connect().await;
        let objects = Arc::new(ObjectRegistry::new(connection.clone()).await.unwrap());
        let backend = Arc::new(MockBackend::with_scanners(scanners));
        let profiles = Arc::new(ProfileRegistry::ephemeral());

        let registry = ScannerRegistry::new(Arc::clone(&objects), store, Arc::clone(&profiles));
        let discovery = Arc::new(Discovery::new(
            Backends::new([Arc::clone(&backend) as Arc<dyn ScannerBackend>]),
            Arc::clone(&registry),
        ));

        for kind in profiles.registered_profiles() {
            objects
                .add(
                    path::profile(kind),
                    Profile1::new(kind, Arc::clone(&profiles)),
                )
                .await
                .unwrap();
        }

        objects
            .add(
                path::manager(),
                Manager1::new(Arc::clone(&discovery), Arc::clone(&profiles)),
            )
            .await
            .unwrap();

        (
            Self {
                scanners: registry,
                discovery,
                objects,
                backend,
            },
            connection,
        )
    }

    fn handle(&self) -> MockHandle {
        self.backend.handle()
    }

    /// Publishes `info` as a paired scanner, the way 4.2's restore path will.
    async fn restore(&self, info: &ScannerInfo) {
        self.scanners
            .register_persistent(
                Arc::clone(&self.backend) as Arc<dyn ScannerBackend>,
                info.clone(),
            )
            .await
            .unwrap();
    }

    /// Publishes `info` as a discovery session would: an object with `Paired=false`.
    async fn sight(&self, info: &ScannerInfo) {
        let ranked = RankedBackend {
            rank: 0,
            backend: Arc::clone(&self.backend) as Arc<dyn ScannerBackend>,
        };
        self.scanners.observe(&ranked, info.clone()).await.unwrap();
    }

    async fn shutdown(&self) {
        self.discovery.stop().await;
        self.scanners.shutdown().await;
        self.objects.shutdown().await;
    }
}

/// A scanner with `count` entries in its physical menu, labels not host-settable — the
/// Brother MFC of §5 when `count` is 4.
fn scanner_with_buttons(address: &str, count: u32) -> ScannerInfo {
    ScannerInfo {
        id: ScannerId::from_backend("mock", address).unwrap(),
        name: "Brother MFC-L2710DW".to_owned(),
        backend: "proprietary:brother".to_owned(),
        address: address.to_owned(),
        capabilities: Capabilities {
            buttons: ButtonsCapability {
                count,
                label_configurable: false,
                labels: Vec::new(),
            },
            ..Capabilities::default()
        },
        status: Status::Online,
    }
}

async fn button_proxy(
    connection: &zbus::Connection,
    id: &ScannerId,
    index: u32,
) -> ButtonProxy<'static> {
    ButtonProxy::builder(connection)
        .path(path::button(id, index))
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap()
}

async fn scanner_proxy(connection: &zbus::Connection, id: &ScannerId) -> ScannerProxy<'static> {
    ScannerProxy::builder(connection)
        .path(path::scanner(id))
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap()
}

/// The `Button1` object paths the manager reports, in tree order — what `busctl tree`
/// shows, minus the structural nodes that carry no interface.
async fn button_paths(connection: &zbus::Connection) -> Vec<OwnedObjectPath> {
    let manager = ObjectManagerProxy::builder(connection)
        .destination(BUS_NAME)
        .unwrap()
        .path(path::manager())
        .unwrap()
        .build()
        .await
        .unwrap();

    let mut paths: Vec<OwnedObjectPath> = manager
        .get_managed_objects()
        .await
        .unwrap()
        .into_iter()
        .filter(|(_, interfaces)| interfaces.contains_key("org.scanbus.Button1"))
        .map(|(path, _)| path)
        .collect();

    paths.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    paths
}

/// The raw `PropertiesChanged` stream for one object.
async fn changes(connection: &zbus::Connection, path: OwnedObjectPath) -> PropertiesChangedStream {
    PropertiesProxy::builder(connection)
        .destination(BUS_NAME)
        .unwrap()
        .path(path)
        .unwrap()
        .build()
        .await
        .unwrap()
        .receive_properties_changed()
        .await
        .unwrap()
}

/// The next value `property` is announced with, skipping signals that do not mention it.
async fn next_change(stream: &mut PropertiesChangedStream, property: &str) -> String {
    let deadline = tokio::time::Instant::now() + SIGNAL_TIMEOUT;

    loop {
        let signal = tokio::time::timeout_at(deadline, stream.next())
            .await
            .unwrap_or_else(|_| panic!("no PropertiesChanged for {property} within the timeout"))
            .expect("the signal stream ended");
        let args = signal.args().unwrap();

        let Some(value) = args.changed_properties.get(property) else {
            continue;
        };

        return match value {
            ZValue::Str(text) => text.to_string(),
            other => panic!("{property} arrived as {other:?}"),
        };
    }
}

/// The error name a call came back with.
fn error_name(error: &zbus::Error) -> String {
    match error {
        zbus::Error::MethodError(name, _, _) => name.as_str().to_owned(),
        zbus::Error::FDO(error) => zbus::DBusError::name(error.as_ref()).as_str().to_owned(),
        other => panic!("expected a named D-Bus error, got {other:?}"),
    }
}

/// `{"output_folder": "<path>"}`, the per-key option §5 gives as its example.
fn output_folder(folder: &str) -> HashMap<String, OwnedValue> {
    HashMap::from([(
        "output_folder".to_owned(),
        OwnedValue::try_from(ZValue::from(folder)).unwrap(),
    )])
}

/// Acceptance: a scanner reporting `buttons.count=4` publishes exactly four button
/// objects, and they carry the read-only half §5 promises.
#[tokio::test]
async fn four_keys_become_four_objects() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("four_keys_become_four_objects");
    };
    let info = scanner_with_buttons("usb:001:002", 4);
    let daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.restore(&info).await;

    assert_eq!(
        button_paths(&client).await,
        (0..4)
            .map(|index| path::button(&info.id, index))
            .collect::<Vec<_>>()
    );

    for index in 0..4 {
        let key = button_proxy(&client, &info.id, index).await;
        assert_eq!(key.index().await.unwrap(), index);
        // No backend seam reports labels yet, so the firmware's is empty and `Label`
        // mirrors it — which is the contract, not a placeholder.
        assert_eq!(key.device_label().await.unwrap(), "");
        assert_eq!(
            key.label().await.unwrap(),
            key.device_label().await.unwrap()
        );
        assert!(!key.label_configurable().await.unwrap());
        assert_eq!(key.profile().await.unwrap(), "");
        assert!(key.profile_options().await.unwrap().is_empty());
    }

    daemon.shutdown().await;
}

/// Acceptance: a scanner with `buttons.count=0` publishes no button objects and pairs
/// normally — a plain flatbed with no walk-up keys is not an error.
#[tokio::test]
async fn a_scanner_without_keys_publishes_none_and_still_pairs() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_scanner_without_keys_publishes_none_and_still_pairs");
    };
    let info = scanner_with_buttons("usb:001:002", 0);
    let daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.sight(&info).await;

    assert!(button_paths(&client).await.is_empty());

    let scanner = scanner_proxy(&client, &info.id).await;
    let mut announced = changes(&client, path::scanner(&info.id)).await;
    scanner.pair(HashMap::new()).await.unwrap();

    assert_eq!(next_change(&mut announced, "PairingState").await, "pairing");
    assert_eq!(next_change(&mut announced, "PairingState").await, "done");
    assert!(scanner.paired().await.unwrap());
    assert!(button_paths(&client).await.is_empty());

    daemon.shutdown().await;
}

/// Acceptance: writing `Label` with `LabelConfigurable=false` returns an error and leaves
/// the property equal to `DeviceLabel`.
#[tokio::test]
async fn a_fixed_label_refuses_the_write_and_keeps_mirroring() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_fixed_label_refuses_the_write_and_keeps_mirroring");
    };
    let info = scanner_with_buttons("usb:001:002", 4);
    let daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.restore(&info).await;

    let key = button_proxy(&client, &info.id, 3).await;
    let error = key
        .set_label("Invoices")
        .await
        .expect_err("§5: a write is refused when the label is the firmware's");
    assert_eq!(
        error_name(&error),
        "org.freedesktop.DBus.Error.PropertyReadOnly"
    );

    assert_eq!(
        key.label().await.unwrap(),
        key.device_label().await.unwrap()
    );

    daemon.shutdown().await;
}

/// Acceptance: writing `Profile="document"` on the key the firmware calls "Scan to OCR"
/// succeeds — the divergence §5 allows on purpose.
///
/// The device label is empty here, since no backend reports one; what the test pins is
/// that nothing about `DeviceLabel` gates the write, and that the write reaches the
/// vendor configuration and is announced.
#[tokio::test]
async fn a_profile_write_reaches_the_backend_and_is_announced() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_profile_write_reaches_the_backend_and_is_announced");
    };
    let info = scanner_with_buttons("usb:001:002", 4);
    let daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.restore(&info).await;

    let key = button_proxy(&client, &info.id, 2).await;
    let mut announced = changes(&client, path::button(&info.id, 2)).await;
    daemon.handle().clear_calls();

    key.set_profile("document").await.unwrap();

    assert_eq!(next_change(&mut announced, "Profile").await, "document");
    assert_eq!(key.profile().await.unwrap(), "document");
    assert_eq!(
        daemon.handle().calls(),
        vec![MockCall::SetButtonMapping {
            scanner: info.id.clone(),
            button_index: 2,
            profile: Some(ProfileKind::Document),
        }]
    );

    // The options half, which the backend records alongside the profile.
    key.set_profile_options(output_folder("~/Scans/invoices"))
        .await
        .unwrap();
    let mapping = daemon.handle().button_mapping(&info.id, 2).unwrap();
    assert_eq!(mapping.profile, ProfileKind::Document);
    assert_eq!(
        mapping.options["output_folder"],
        scanbus_core::Value::Str("~/Scans/invoices".to_owned())
    );
    assert_eq!(
        String::try_from(key.profile_options().await.unwrap()["output_folder"].clone()).unwrap(),
        "~/Scans/invoices"
    );

    // And `""` clears the key, in the vendor config as well as here.
    key.set_profile("").await.unwrap();
    assert_eq!(key.profile().await.unwrap(), "");
    assert_eq!(daemon.handle().button_mapping(&info.id, 2), None);

    daemon.shutdown().await;
}

/// Acceptance: with the mock's `set_button_mapping` made to fail, the write returns the
/// error and `Profile` still reads its previous value.
#[tokio::test]
async fn a_backend_that_refuses_leaves_the_property_where_it_was() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_backend_that_refuses_leaves_the_property_where_it_was");
    };
    let info = scanner_with_buttons("usb:001:002", 4);
    let daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.restore(&info).await;

    let key = button_proxy(&client, &info.id, 1).await;
    key.set_profile("image").await.unwrap();

    // 5.4's condition, without needing a read-only /opt to reproduce it.
    daemon.handle().fail_button_mapping(BackendError::Other(
        "/opt/brother/scanner/brscan5/brsanenetdevice4.cfg is not writable".to_owned(),
    ));

    let error = key
        .set_profile("document")
        .await
        .expect_err("a config the backend could not rewrite is not a successful write");
    assert_eq!(error_name(&error), "org.freedesktop.DBus.Error.Failed");
    assert!(error.to_string().contains("not writable"), "{error}");

    assert_eq!(key.profile().await.unwrap(), "image");
    assert_eq!(
        daemon.handle().button_mapping(&info.id, 1).unwrap().profile,
        ProfileKind::Image
    );

    daemon.shutdown().await;
}

/// Checklist: a `Profile` outside `GetProfileTypes` is refused before any backend call.
#[tokio::test]
async fn an_unrunnable_profile_is_refused_before_the_backend_is_asked() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("an_unrunnable_profile_is_refused_before_the_backend_is_asked");
    };
    let info = scanner_with_buttons("usb:001:002", 4);
    let daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.restore(&info).await;

    let key = button_proxy(&client, &info.id, 0).await;
    daemon.handle().clear_calls();

    // `ocr` and `email` parse — §6 lists all four and a config UI may hold one — and this
    // pipeline runs neither; `pdf` is not a profile name at all. All three are the same
    // refusal, and §8's name for it (`UnsupportedProfile`) is one a `Set` reply cannot
    // carry, so it arrives as `InvalidArgs` with the list in the message.
    for profile in ["ocr", "email", "pdf"] {
        let Err(error) = key.set_profile(profile).await else {
            panic!("{profile} is outside GetProfileTypes and must be refused");
        };
        assert_eq!(error_name(&error), "org.freedesktop.DBus.Error.InvalidArgs");
        assert!(error.to_string().contains("image, document"), "{error}");
    }

    assert_eq!(key.profile().await.unwrap(), "");
    assert!(
        daemon.handle().calls().is_empty(),
        "a refused profile must not have reached the vendor config: {:?}",
        daemon.handle().calls()
    );

    daemon.shutdown().await;
}

/// Checklist: the buttons go away with their scanner, and come back with it.
#[tokio::test]
async fn buttons_are_removed_with_the_scanner() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("buttons_are_removed_with_the_scanner");
    };
    let info = scanner_with_buttons("usb:001:002", 4);
    let daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;

    // A scanner that exists only because a probe saw it: `StopDiscovery` takes it, and
    // its keys, off the bus.
    daemon.sight(&info).await;
    assert_eq!(button_paths(&client).await.len(), 4);

    assert_eq!(daemon.scanners.end_discovery().await, 1);
    assert!(button_paths(&client).await.is_empty());

    // And a paired one: `Unpair()` removes the object no session sees, children first.
    daemon.restore(&info).await;
    assert_eq!(button_paths(&client).await.len(), 4);

    scanner_proxy(&client, &info.id)
        .await
        .unpair()
        .await
        .unwrap();
    assert!(button_paths(&client).await.is_empty());

    daemon.shutdown().await;
}

/// Checklist: a rediscovery that reports a different `buttons.count` moves the tree with
/// it, and the keys that survive keep what was assigned to them.
#[tokio::test]
async fn a_menu_that_changes_size_moves_the_tree() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_menu_that_changes_size_moves_the_tree");
    };
    let two_keys = scanner_with_buttons("usb:001:002", 2);
    let daemon = Daemon::start(&bus, [two_keys.clone()]).await;
    let client = bus.connect().await;
    daemon.sight(&two_keys).await;

    assert_eq!(button_paths(&client).await.len(), 2);
    button_proxy(&client, &two_keys.id, 0)
        .await
        .set_profile("image")
        .await
        .unwrap();

    // The backend looks again and reports the four keys the device really has.
    let four_keys = scanner_with_buttons("usb:001:002", 4);
    daemon.sight(&four_keys).await;

    assert_eq!(button_paths(&client).await.len(), 4);
    assert_eq!(
        button_proxy(&client, &four_keys.id, 0)
            .await
            .profile()
            .await
            .unwrap(),
        "image",
        "a key that survived the change kept its assignment"
    );

    // And back down again: the tail goes, the survivor stays.
    daemon.sight(&two_keys).await;
    assert_eq!(button_paths(&client).await.len(), 2);
    assert_eq!(
        button_proxy(&client, &two_keys.id, 0)
            .await
            .profile()
            .await
            .unwrap(),
        "image"
    );

    daemon.shutdown().await;
}

/// A scanner as the store holds it, naming the backend this daemon was built with.
///
/// The restore path resolves `ScannerInfo::backend` against the compiled backends, so a
/// persisted `proprietary:brother` would be skipped here as a backend this binary does
/// not have — a different case from the one these tests are about.
fn persisted_scanner(address: &str, count: u32) -> ScannerInfo {
    ScannerInfo {
        backend: MockBackend::ID.to_owned(),
        ..scanner_with_buttons(address, count)
    }
}

/// The store a previous run would have left behind: `info` paired, and key `index`
/// assigned `profile` with the per-key option §5 gives as its example.
async fn store_with_assignment(
    dir: &TempDir,
    info: &ScannerInfo,
    index: u32,
    profile: ProfileKind,
) -> Arc<JsonPairingStore> {
    let store = Arc::new(JsonPairingStore::with_path(
        dir.path().join("pairings.json"),
    ));
    store.save_paired(info).await.unwrap();

    let mut button = ButtonInfo::new(index, "", false);
    button.profile = Some(profile);
    button.profile_options = BTreeMap::from([(
        "output_folder".to_owned(),
        scanbus_core::Value::Str("~/Scans/invoices".to_owned()),
    )]);
    store.save_button(&info.id, &button).await.unwrap();

    store
}

/// Every `SetButtonMapping` the backend was asked for, in order.
fn registrations(handle: &MockHandle) -> Vec<MockCall> {
    handle
        .calls()
        .into_iter()
        .filter(|call| matches!(call, MockCall::SetButtonMapping { .. }))
        .collect()
}

/// Acceptance (5.11): restarting the daemon restores every previously assigned entry with
/// no user action, and no entry the user had cleared.
///
/// Nothing is discovered here — the store is the whole of what this daemon knows, which
/// is exactly the state a restart starts from. On a backend where the mapping *is* the
/// panel entry, the `SetButtonMapping` asserted below is the entry coming back onto the
/// device's menu.
#[tokio::test]
async fn a_restart_puts_the_assignments_back_on_the_keys_and_on_the_device() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_restart_puts_the_assignments_back_on_the_keys_and_on_the_device");
    };
    let info = persisted_scanner("usb:001:002", 4);
    let dir = TempDir::new().unwrap();
    let store = store_with_assignment(&dir, &info, 1, ProfileKind::Image).await;

    let daemon = Daemon::start_restoring(&bus, store, [info.clone()]).await;
    let client = bus.connect().await;

    let key = button_proxy(&client, &info.id, 1).await;
    assert_eq!(key.profile().await.unwrap(), "image");
    assert_eq!(
        String::try_from(key.profile_options().await.unwrap()["output_folder"].clone()).unwrap(),
        "~/Scans/invoices",
        "the options are restored with the profile, not left behind by it"
    );

    assert_eq!(
        registrations(&daemon.handle()),
        vec![MockCall::SetButtonMapping {
            scanner: info.id.clone(),
            button_index: 1,
            profile: Some(ProfileKind::Image),
        }],
        "only the assigned key is registered: the three nobody wrote to stay off the panel"
    );
    assert_eq!(
        daemon.handle().button_mapping(&info.id, 1).unwrap().options["output_folder"],
        scanbus_core::Value::Str("~/Scans/invoices".to_owned())
    );

    for index in [0, 2, 3] {
        let key = button_proxy(&client, &info.id, index).await;
        assert_eq!(key.profile().await.unwrap(), "");
        assert!(key.profile_options().await.unwrap().is_empty());
    }

    daemon.shutdown().await;
}

/// Checklist: the printer was switched off when the daemon started. The registration
/// fails and the assignment is still there — it is the user's configuration, in the
/// store, and a device that could not be reached is not a reason to delete it.
#[tokio::test]
async fn a_device_that_refuses_at_startup_keeps_its_assignment() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_device_that_refuses_at_startup_keeps_its_assignment");
    };
    let info = persisted_scanner("usb:001:002", 4);
    let dir = TempDir::new().unwrap();
    let store = store_with_assignment(&dir, &info, 1, ProfileKind::Image).await;

    let daemon = Daemon::start_restoring_with(&bus, Arc::clone(&store), [info.clone()], |handle| {
        handle.fail_button_mapping(BackendError::NotReachable {
            scanner: ScannerId::from_backend(MockBackend::ID, "usb:001:002")
                .expect("a legal mock id"),
            detail: "the printer did not answer".to_owned(),
        });
    })
    .await;
    let client = bus.connect().await;

    // The daemon came up regardless: the name is taken and the keys are exported.
    assert_eq!(button_paths(&client).await.len(), 4);

    let key = button_proxy(&client, &info.id, 1).await;
    assert_eq!(
        key.profile().await.unwrap(),
        "image",
        "the property reads what was persisted, although the device refused it"
    );
    assert_eq!(
        daemon.handle().button_mapping(&info.id, 1),
        None,
        "and the device really did not take it"
    );
    assert_eq!(
        store.restorable().await.unwrap()[0].buttons[0].profile,
        Some(ProfileKind::Image),
        "the store still has it, for the next start with the device answering"
    );

    daemon.shutdown().await;
}

/// Checklist: the store remembers a key the device no longer reports — a backend that
/// could not read the panel this time round. The startup skips it and leaves the
/// assignment where it is, rather than failing or dropping it.
#[tokio::test]
async fn an_assignment_for_a_key_that_is_gone_is_left_in_the_store() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("an_assignment_for_a_key_that_is_gone_is_left_in_the_store");
    };
    let info = persisted_scanner("usb:001:002", 2);
    let dir = TempDir::new().unwrap();
    let store = store_with_assignment(&dir, &info, 3, ProfileKind::Document).await;

    let daemon = Daemon::start_restoring(&bus, Arc::clone(&store), [info.clone()]).await;
    let client = bus.connect().await;

    assert_eq!(button_paths(&client).await.len(), 2);
    assert!(
        registrations(&daemon.handle()).is_empty(),
        "a key with no object is not registered on a device that no longer offers it"
    );
    assert_eq!(
        store.restorable().await.unwrap()[0].buttons[0].profile,
        Some(ProfileKind::Document),
        "the assignment survives, for a probe that reports the key again"
    );

    daemon.shutdown().await;
}
