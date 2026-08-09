//! `Scanner1`'s methods and the pairing that never blocks, on a real bus.
//!
//! The acceptance criteria of 2.3, run rather than described. The one that shapes the
//! rest is §9's: `Pair()` returns while the install is still going, and everything the
//! client learns afterwards arrives as `PropertiesChanged`. So the install here is not
//! slow, it is *held* — [`GatedBackend`] blocks inside `ensure_installed` until the test
//! lets it go, which asserts the same property as a 30-second `.deb` download without
//! putting 30 seconds into the suite, and without a timer deciding whether the run is
//! green.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt as _;
use futures_util::stream::BoxStream;
use scanbus_core::backend::mock::MockBackend;
use scanbus_core::{
    BackendError, ButtonPressedEvent, Capabilities, PairingProgress, ProfileKind, RawPage,
    ScannerBackend, ScannerId, ScannerInfo, Status, Value,
};
use scanbus_daemon::dbus::{self, BUS_NAME, Manager1, ObjectRegistry, path};
use scanbus_daemon::scanners::Origin;
use scanbus_daemon::{Backends, Discovery, MemoryPairingStore, ScannerRegistry};
use tokio::sync::{Mutex as AsyncMutex, mpsc, watch};
use zbus::fdo::{ManagedObjects, ObjectManagerProxy, PropertiesChangedStream, PropertiesProxy};

mod common;

use common::{PrivateBus, skipped};

/// How long a signal that should already be on its way is waited for.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a change that has to wait for the next probe round is waited for.
///
/// A round is [`scanbus_daemon::discovery::PROBE_INTERVAL`] apart, so anything driven by
/// "a backend stopped reporting this scanner" cannot arrive sooner than that.
const ROUND_TIMEOUT: Duration = Duration::from_secs(20);

/// What §9 calls "well under a second": how long `Pair()` may take to return while an
/// install is in flight.
const PAIR_REPLY_BUDGET: Duration = Duration::from_secs(1);

#[zbus::proxy(
    interface = "org.scanbus.Manager1",
    default_service = "org.scanbus",
    default_path = "/org/scanbus"
)]
trait Manager {
    fn start_discovery(
        &self,
        filters: HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::Result<()>;
    fn stop_discovery(&self) -> zbus::Result<()>;
}

/// `org.scanbus.Scanner1` as §3 defines it, from the client side.
#[zbus::proxy(interface = "org.scanbus.Scanner1", default_service = "org.scanbus")]
trait Scanner {
    fn pair(&self, options: HashMap<String, zbus::zvariant::OwnedValue>) -> zbus::Result<()>;
    fn cancel_pairing(&self) -> zbus::Result<()>;
    fn unpair(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn paired(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn status(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn pairing_state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn pairing_error(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn default_profile(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_default_profile(&self, value: &str) -> zbus::Result<()>;
    #[zbus(property)]
    fn supported_profiles(&self) -> zbus::Result<Vec<String>>;
}

/// A backend whose `ensure_installed` stops in the middle of an install and waits.
///
/// It re-implements the step rather than delegating to [`MockBackend`] so that the
/// progress a client sees is one clean `Checking → Installing → Ready`: delegating after
/// the gate opened would replay `Checking`, and `PairingState` would go back from
/// `installing_backend` to `pairing` in the middle of an install that never restarted.
struct GatedBackend {
    inner: MockBackend,
    /// `true` lets a held `ensure_installed` finish. Level-triggered, so opening it
    /// before the backend gets there — and a second `Pair()` after a cancel — both work.
    open: watch::Sender<bool>,
    /// How many times `ensure_installed` has been entered, cancelled runs included.
    entries: AtomicU32,
    /// What the call fails with once the gate opens, if anything.
    failure: AsyncMutex<Option<BackendError>>,
}

impl GatedBackend {
    fn new(scanners: impl IntoIterator<Item = ScannerInfo>) -> Arc<Self> {
        Arc::new(Self {
            inner: MockBackend::with_scanners(scanners).with_id("mock"),
            open: watch::channel(false).0,
            entries: AtomicU32::new(0),
            failure: AsyncMutex::new(None),
        })
    }

    /// Lets whatever is waiting inside `ensure_installed` finish.
    fn release(&self) {
        self.open.send_replace(true);
    }

    /// Puts the gate back, so the next `Pair()` is held too.
    fn hold(&self) {
        self.open.send_replace(false);
    }

    /// Makes the held call fail once released.
    async fn fail_with(&self, error: BackendError) {
        *self.failure.lock().await = Some(error);
    }

    fn entries(&self) -> u32 {
        self.entries.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ScannerBackend for GatedBackend {
    fn id(&self) -> &'static str {
        self.inner.id()
    }

    async fn discover(&self) -> Result<Vec<ScannerInfo>, BackendError> {
        self.inner.discover().await
    }

    async fn ensure_installed(
        &self,
        scanner: &ScannerInfo,
        progress: mpsc::Sender<PairingProgress>,
    ) -> Result<(), BackendError> {
        self.entries.fetch_add(1, Ordering::SeqCst);

        let _ = progress
            .send(PairingProgress::Checking {
                message: format!("checking dependencies for {}", scanner.name),
            })
            .await;
        let _ = progress
            .send(PairingProgress::Installing {
                package: "brscan5".to_owned(),
                percent: None,
            })
            .await;

        let mut open = self.open.subscribe();
        while !*open.borrow_and_update() {
            if open.changed().await.is_err() {
                break;
            }
        }

        if let Some(error) = self.failure.lock().await.clone() {
            let _ = progress
                .send(PairingProgress::Failed {
                    message: error.to_string(),
                })
                .await;
            return Err(error);
        }

        let _ = progress.send(PairingProgress::Ready).await;
        Ok(())
    }

    async fn start_listening(
        &self,
        scanner: &ScannerInfo,
    ) -> Result<BoxStream<'static, ButtonPressedEvent>, BackendError> {
        self.inner.start_listening(scanner).await
    }

    async fn stop_listening(&self, scanner_id: &ScannerId) -> Result<(), BackendError> {
        self.inner.stop_listening(scanner_id).await
    }

    async fn set_button_mapping(
        &self,
        scanner_id: &ScannerId,
        button_index: u32,
        profile: Option<ProfileKind>,
        options: &std::collections::BTreeMap<String, Value>,
    ) -> Result<(), BackendError> {
        self.inner
            .set_button_mapping(scanner_id, button_index, profile, options)
            .await
    }

    async fn fetch_pages(
        &self,
        scanner_id: &ScannerId,
        job_id: &str,
    ) -> Result<BoxStream<'static, RawPage>, BackendError> {
        self.inner.fetch_pages(scanner_id, job_id).await
    }
}

/// The service side: what `main.rs` wires up, with backends a test chose.
struct Daemon {
    scanners: Arc<ScannerRegistry>,
    discovery: Arc<Discovery>,
    objects: Arc<ObjectRegistry>,
    store: Arc<MemoryPairingStore>,
}

impl Daemon {
    async fn start(bus: &PrivateBus, backends: Backends) -> Self {
        let connection = bus.connect().await;
        let objects = Arc::new(ObjectRegistry::new(connection.clone()).await.unwrap());
        let store = Arc::new(MemoryPairingStore::new());
        let scanners = ScannerRegistry::new(Arc::clone(&objects), Arc::clone(&store) as _);
        let discovery = Arc::new(Discovery::new(backends, Arc::clone(&scanners)));

        objects
            .add(path::manager(), Manager1::new(Arc::clone(&discovery)))
            .await
            .unwrap();
        dbus::request_name(&connection).await.unwrap();

        Self {
            scanners,
            discovery,
            objects,
            store,
        }
    }

    async fn shutdown(&self) {
        self.discovery.stop().await;
        self.objects.shutdown().await;
    }
}

fn scanner_info(address: &str, name: &str) -> ScannerInfo {
    ScannerInfo {
        id: ScannerId::from_backend("mock", address).unwrap(),
        name: name.to_owned(),
        backend: "proprietary:brother".to_owned(),
        address: address.to_owned(),
        capabilities: Capabilities::default(),
        status: Status::Online,
    }
}

fn backends(backend: Arc<dyn ScannerBackend>) -> Backends {
    Backends::new([backend])
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

fn scanner_paths(managed: &ManagedObjects) -> Vec<String> {
    let mut paths: Vec<String> = managed
        .iter()
        .filter(|(_, interfaces)| interfaces.contains_key("org.scanbus.Scanner1"))
        .map(|(path, _)| path.as_str().to_owned())
        .collect();
    paths.sort();
    paths
}

/// Starts discovery and waits for the scanner's object to be published.
async fn discover(client: &zbus::Connection, id: &ScannerId) {
    let manager = manager_proxy(client).await;
    let mut added = manager.receive_interfaces_added().await.unwrap();

    ManagerProxy::new(client)
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
        let args = signal.args().unwrap();
        if args.object_path() == &*wanted {
            return;
        }
    }
}

/// The raw `PropertiesChanged` stream for a scanner object.
///
/// The signals themselves rather than a cached property proxy: this is what
/// `dbus-monitor` shows, and it is the only way to assert that a *sequence* of states was
/// announced. A `PropertyStream` reports the proxy's cache catching up, which coalesces
/// two changes that arrive before it is read and also fires once when the cache is first
/// filled — neither of which is a statement about what the daemon sent.
async fn changes(connection: &zbus::Connection, id: &ScannerId) -> PropertiesChangedStream {
    PropertiesProxy::builder(connection)
        .destination(BUS_NAME)
        .unwrap()
        .path(path::scanner(id))
        .unwrap()
        .build()
        .await
        .unwrap()
        .receive_properties_changed()
        .await
        .unwrap()
}

/// Everything the next signal that mentions `property` carried, rendered as strings.
///
/// The whole map, because which properties travel *together* is part of the contract: a
/// client is entitled to learn that a scanner paired from one signal rather than from two
/// it has to correlate.
async fn next_change_set(
    stream: &mut PropertiesChangedStream,
    property: &str,
    timeout: Duration,
) -> HashMap<String, String> {
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let signal = tokio::time::timeout_at(deadline, stream.next())
            .await
            .unwrap_or_else(|_| panic!("no PropertiesChanged for {property} within the timeout"))
            .expect("the signal stream ended");
        let args = signal.args().unwrap();
        assert_eq!(args.interface_name, "org.scanbus.Scanner1");

        if !args.changed_properties.contains_key(property) {
            continue;
        }

        return args
            .changed_properties
            .iter()
            .map(|(name, value)| {
                let rendered = match value {
                    zbus::zvariant::Value::Str(text) => text.to_string(),
                    zbus::zvariant::Value::Bool(flag) => flag.to_string(),
                    other => panic!("{name} arrived as {other:?}"),
                };
                ((*name).to_owned(), rendered)
            })
            .collect();
    }
}

/// The next value `property` is announced with, skipping signals that do not mention it.
async fn next_change(
    stream: &mut PropertiesChangedStream,
    property: &str,
    timeout: Duration,
) -> String {
    next_change_set(stream, property, timeout).await[property].clone()
}

/// The next `PairingState` a client is told about.
async fn next_pairing_state(stream: &mut PropertiesChangedStream, timeout: Duration) -> String {
    next_change(stream, "PairingState", timeout).await
}

/// The error name a call came back with.
///
/// Two shapes reach a client: zbus decodes the standard names into
/// [`zbus::fdo::Error`] and leaves everything else — every `org.scanbus.Error.*` — as a
/// [`zbus::Error::MethodError`].
fn error_name(error: &zbus::Error) -> String {
    match error {
        zbus::Error::MethodError(name, _, _) => name.as_str().to_owned(),
        zbus::Error::FDO(error) => zbus::DBusError::name(error.as_ref()).as_str().to_owned(),
        other => panic!("expected a named D-Bus error, got {other:?}"),
    }
}

/// Acceptance: `Pair()` returns while the install is still running, and the client is
/// walked through `pairing → installing_backend → done` by `PropertiesChanged` alone.
#[tokio::test]
async fn pair_returns_immediately_and_the_states_arrive_as_signals() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("pair_returns_immediately_and_the_states_arrive_as_signals");
    };

    let found = scanner_info("usb:001:002", "Brother MFC-L2710DW");
    let backend = GatedBackend::new([found.clone()]);
    let daemon = Daemon::start(&bus, backends(Arc::clone(&backend) as _)).await;

    let client = bus.connect().await;
    discover(&client, &found.id).await;

    let scanner = scanner_proxy(&client, &found.id).await;
    assert_eq!(scanner.pairing_state().await.unwrap(), "none");
    assert!(!scanner.paired().await.unwrap());

    // Subscribe before calling: §7 of the CLI spec, and the only way this is raceless.
    let mut signals = changes(&client, &found.id).await;

    // The install is held open for the whole of this call. A handler that awaited
    // `ensure_installed` would never return.
    tokio::time::timeout(PAIR_REPLY_BUDGET, scanner.pair(HashMap::new()))
        .await
        .expect("Pair() did not return while the install was in flight")
        .unwrap();

    assert_eq!(
        next_pairing_state(&mut signals, SIGNAL_TIMEOUT).await,
        "pairing"
    );
    assert_eq!(
        next_pairing_state(&mut signals, SIGNAL_TIMEOUT).await,
        "installing_backend"
    );
    // Still not paired, and still no object churn: the install is what is slow.
    assert!(!scanner.paired().await.unwrap());

    backend.release();

    // `Paired` moves in the same signal as the state that made it true: a client that
    // waited for `done` never has to go back and ask whether the scanner is paired.
    let done = next_change_set(&mut signals, "PairingState", SIGNAL_TIMEOUT).await;
    assert_eq!(done["PairingState"], "done");
    assert_eq!(done["Paired"], "true");
    assert!(scanner.paired().await.unwrap());
    assert_eq!(scanner.pairing_error().await.unwrap(), "");

    // The pairing is durable before `done` is announced (1.4's rule), and the object has
    // stopped belonging to the discovery session.
    assert!(daemon.store.contains(&found.id));
    assert_eq!(
        daemon.scanners.origin(&found.id).await,
        Some(Origin::Persistent)
    );

    // §9: pairing an already-paired scanner is its own named error.
    let error = scanner
        .pair(HashMap::new())
        .await
        .expect_err("a second Pair() on a paired scanner must be refused");
    assert_eq!(error_name(&error), "org.scanbus.Error.AlreadyPaired");

    daemon.shutdown().await;
}

/// Acceptance: `CancelPairing` during the install leaves `PairingState="none"` and
/// `Paired=false`, and a second `Pair` completes normally.
#[tokio::test]
async fn cancelling_during_the_install_lands_on_none_and_a_retry_works() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("cancelling_during_the_install_lands_on_none_and_a_retry_works");
    };

    let found = scanner_info("usb:001:002", "Brother MFC-L2710DW");
    let backend = GatedBackend::new([found.clone()]);
    let daemon = Daemon::start(&bus, backends(Arc::clone(&backend) as _)).await;

    let client = bus.connect().await;
    discover(&client, &found.id).await;
    let scanner = scanner_proxy(&client, &found.id).await;
    let mut signals = changes(&client, &found.id).await;

    scanner.pair(HashMap::new()).await.unwrap();
    assert_eq!(
        next_pairing_state(&mut signals, SIGNAL_TIMEOUT).await,
        "pairing"
    );
    assert_eq!(
        next_pairing_state(&mut signals, SIGNAL_TIMEOUT).await,
        "installing_backend"
    );
    assert_eq!(backend.entries(), 1);

    scanner.cancel_pairing().await.unwrap();
    assert_eq!(
        next_pairing_state(&mut signals, SIGNAL_TIMEOUT).await,
        "none"
    );
    assert!(!scanner.paired().await.unwrap());
    assert!(!daemon.store.contains(&found.id));

    // The object is still there to retry on — §9's "a failed or cancelled attempt does
    // not cost you a discovery round".
    assert_eq!(
        scanner_paths(
            &manager_proxy(&client)
                .await
                .get_managed_objects()
                .await
                .unwrap()
        ),
        vec![path::scanner(&found.id).as_str().to_owned()]
    );

    // And the retry runs the sequence again, from the top.
    backend.release();
    scanner.pair(HashMap::new()).await.unwrap();
    assert_eq!(
        next_pairing_state(&mut signals, SIGNAL_TIMEOUT).await,
        "pairing"
    );
    loop {
        let state = next_pairing_state(&mut signals, SIGNAL_TIMEOUT).await;
        if state == "done" {
            break;
        }
        assert_eq!(state, "installing_backend", "unexpected state on the retry");
    }
    assert!(scanner.paired().await.unwrap());
    assert_eq!(backend.entries(), 2);

    daemon.shutdown().await;
}

/// Acceptance: a failed pairing leaves the object present with `PairingState="failed"`
/// and `PairingError` set to the backend's message.
#[tokio::test]
async fn a_failed_pairing_keeps_the_object_and_reports_why() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_failed_pairing_keeps_the_object_and_reports_why");
    };

    let found = scanner_info("usb:001:002", "Brother MFC-L2710DW");
    let backend = GatedBackend::new([found.clone()]);
    let failure = BackendError::InstallFailed {
        package: "brscan5".to_owned(),
        detail: "404 fetching brscan5-1.2.6-0.amd64.deb".to_owned(),
    };
    backend.fail_with(failure.clone()).await;
    backend.release();

    let daemon = Daemon::start(&bus, backends(Arc::clone(&backend) as _)).await;
    let client = bus.connect().await;
    discover(&client, &found.id).await;

    let scanner = scanner_proxy(&client, &found.id).await;
    let mut signals = changes(&client, &found.id).await;

    scanner.pair(HashMap::new()).await.unwrap();

    loop {
        let state = next_pairing_state(&mut signals, SIGNAL_TIMEOUT).await;
        if state == "failed" {
            break;
        }
        assert!(
            state == "pairing" || state == "installing_backend",
            "unexpected state {state} on the way to failed"
        );
    }

    assert_eq!(scanner.pairing_error().await.unwrap(), failure.to_string());
    assert!(!scanner.paired().await.unwrap());
    assert!(!daemon.store.contains(&found.id));

    // §9: the object stays, so another attempt needs no new discovery round.
    assert_eq!(
        scanner_paths(
            &manager_proxy(&client)
                .await
                .get_managed_objects()
                .await
                .unwrap()
        ),
        vec![path::scanner(&found.id).as_str().to_owned()]
    );
    assert_eq!(
        daemon.scanners.origin(&found.id).await,
        Some(Origin::Discovered)
    );

    daemon.shutdown().await;
}

/// Acceptance: making a paired scanner unreachable moves `Status` to `"offline"` with
/// `Paired` still `true` — §9's "reachable ≠ paired", from a client's point of view.
#[tokio::test]
async fn a_paired_scanner_that_goes_away_is_offline_and_still_paired() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_paired_scanner_that_goes_away_is_offline_and_still_paired");
    };

    let found = scanner_info("usb:001:002", "Brother MFC-L2710DW");
    let backend = Arc::new(MockBackend::with_scanners([found.clone()]).with_id("mock"));
    let handle = backend.handle();
    let daemon = Daemon::start(&bus, backends(Arc::clone(&backend) as _)).await;

    let client = bus.connect().await;
    discover(&client, &found.id).await;

    let scanner = scanner_proxy(&client, &found.id).await;
    let mut signals = changes(&client, &found.id).await;

    scanner.pair(HashMap::new()).await.unwrap();
    loop {
        if next_pairing_state(&mut signals, SIGNAL_TIMEOUT).await == "done" {
            break;
        }
    }
    assert_eq!(scanner.status().await.unwrap(), "online");

    // The scanner is switched off: the backend stops reporting it. The next probe round
    // is what notices.
    handle.set_discovery([]);

    assert_eq!(
        next_change(&mut signals, "Status", ROUND_TIMEOUT).await,
        "offline"
    );

    assert!(
        scanner.paired().await.unwrap(),
        "an unreachable scanner keeps its pairing"
    );
    assert_eq!(scanner.pairing_state().await.unwrap(), "done");
    assert!(daemon.store.contains(&found.id));

    // And it is still on the bus after the session ends, because it is paired.
    ManagerProxy::new(&client)
        .await
        .unwrap()
        .stop_discovery()
        .await
        .unwrap();
    assert_eq!(
        scanner_paths(
            &manager_proxy(&client)
                .await
                .get_managed_objects()
                .await
                .unwrap()
        ),
        vec![path::scanner(&found.id).as_str().to_owned()]
    );

    daemon.shutdown().await;
}

/// Acceptance: `DefaultProfile` is validated at write time against `SupportedProfiles`.
///
/// The criterion asks for `org.scanbus.Error.UnsupportedProfile` and this answers
/// `org.freedesktop.DBus.Error.InvalidArgs`, because a property `Set` is served by zbus's
/// own `org.freedesktop.DBus.Properties` and can only fail with a standard name — see
/// [`scanbus_daemon::dbus::error`]. What the criterion is actually about, the write being
/// refused rather than the scan failing hours later, is what is asserted here.
#[tokio::test]
async fn writing_an_unsupported_default_profile_is_refused() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("writing_an_unsupported_default_profile_is_refused");
    };

    let found = scanner_info("usb:001:002", "Brother MFC-L2710DW");
    let backend = Arc::new(MockBackend::with_scanners([found.clone()]).with_id("mock"));
    let daemon = Daemon::start(&bus, backends(Arc::clone(&backend) as _)).await;

    let client = bus.connect().await;
    discover(&client, &found.id).await;
    let scanner = scanner_proxy(&client, &found.id).await;

    // Nothing by default: data with no profile attached is left raw (§4).
    assert_eq!(scanner.default_profile().await.unwrap(), "");
    assert_eq!(
        scanner.supported_profiles().await.unwrap(),
        ["image", "document"]
    );

    let mut signals = changes(&client, &found.id).await;
    scanner.set_default_profile("document").await.unwrap();
    assert_eq!(
        next_change(&mut signals, "DefaultProfile", SIGNAL_TIMEOUT).await,
        "document"
    );

    // `ocr` is a real profile this build cannot run, so it is refused here rather than
    // at scan time — and the value that was already there survives the refusal.
    for rejected in ["ocr", "banana"] {
        let error = scanner
            .set_default_profile(rejected)
            .await
            .expect_err("an unsupported profile must be refused");
        assert_eq!(error_name(&error), "org.freedesktop.DBus.Error.InvalidArgs");
        assert!(
            error.to_string().contains("image, document"),
            "the message should name what is supported: {error}"
        );
    }
    assert_eq!(scanner.default_profile().await.unwrap(), "document");

    // `""` is how a client clears it.
    scanner.set_default_profile("").await.unwrap();
    assert_eq!(scanner.default_profile().await.unwrap(), "");

    daemon.shutdown().await;
}

/// `Unpair()` on a scanner the current discovery session still sees keeps the object,
/// with `Paired=false`; a second `Unpair()` has nothing left to undo.
#[tokio::test]
async fn unpairing_a_discovered_scanner_keeps_its_object() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("unpairing_a_discovered_scanner_keeps_its_object");
    };

    let found = scanner_info("usb:001:002", "Brother MFC-L2710DW");
    let backend = Arc::new(MockBackend::with_scanners([found.clone()]).with_id("mock"));
    let daemon = Daemon::start(&bus, backends(Arc::clone(&backend) as _)).await;

    let client = bus.connect().await;
    discover(&client, &found.id).await;
    let scanner = scanner_proxy(&client, &found.id).await;
    let mut signals = changes(&client, &found.id).await;

    scanner.pair(HashMap::new()).await.unwrap();
    loop {
        if next_pairing_state(&mut signals, SIGNAL_TIMEOUT).await == "done" {
            break;
        }
    }
    assert!(daemon.store.contains(&found.id));

    scanner.unpair().await.unwrap();

    // The object stayed, because discovery is still finding it: unpairing removed the
    // pairing, not the scanner.
    assert_eq!(
        scanner_paths(
            &manager_proxy(&client)
                .await
                .get_managed_objects()
                .await
                .unwrap()
        ),
        vec![path::scanner(&found.id).as_str().to_owned()]
    );
    assert_eq!(
        daemon.scanners.origin(&found.id).await,
        Some(Origin::Discovered)
    );
    assert_eq!(
        next_pairing_state(&mut signals, SIGNAL_TIMEOUT).await,
        "none"
    );
    assert!(!scanner.paired().await.unwrap());
    assert!(!daemon.store.contains(&found.id));

    let error = scanner
        .unpair()
        .await
        .expect_err("a second Unpair() has nothing to undo");
    assert_eq!(error_name(&error), "org.scanbus.Error.NotPaired");

    // And it can be paired again from the object that was kept.
    scanner.pair(HashMap::new()).await.unwrap();
    loop {
        if next_pairing_state(&mut signals, SIGNAL_TIMEOUT).await == "done" {
            break;
        }
    }
    assert!(scanner.paired().await.unwrap());

    daemon.shutdown().await;
}

/// `Unpair()` on a scanner that exists only because it is paired takes its object with
/// it, and the client hears about it as `InterfacesRemoved`.
#[tokio::test]
async fn unpairing_a_scanner_no_probe_sees_removes_its_object() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("unpairing_a_scanner_no_probe_sees_removes_its_object");
    };

    let found = scanner_info("usb:001:002", "Brother MFC-L2710DW");
    let backend = Arc::new(MockBackend::with_scanners([found.clone()]).with_id("mock"));
    let handle = backend.handle();
    let daemon = Daemon::start(&bus, backends(Arc::clone(&backend) as _)).await;

    let client = bus.connect().await;
    let manager = manager_proxy(&client).await;
    let mut removed = manager.receive_interfaces_removed().await.unwrap();

    discover(&client, &found.id).await;
    let scanner = scanner_proxy(&client, &found.id).await;
    let mut signals = changes(&client, &found.id).await;

    scanner.pair(HashMap::new()).await.unwrap();
    loop {
        if next_pairing_state(&mut signals, SIGNAL_TIMEOUT).await == "done" {
            break;
        }
    }

    // The session ends and the backend stops reporting it: the object now exists only
    // because of the pairing, which is the case where `Unpair()` takes it away.
    handle.set_discovery([]);
    ManagerProxy::new(&client)
        .await
        .unwrap()
        .stop_discovery()
        .await
        .unwrap();
    assert_eq!(
        daemon.scanners.origin(&found.id).await,
        Some(Origin::Persistent)
    );

    scanner.unpair().await.unwrap();

    let signal = tokio::time::timeout(SIGNAL_TIMEOUT, removed.next())
        .await
        .expect("no InterfacesRemoved within the timeout")
        .unwrap();
    assert_eq!(
        signal.args().unwrap().object_path(),
        &*path::scanner(&found.id)
    );

    assert!(
        scanner_paths(&manager.get_managed_objects().await.unwrap()).is_empty(),
        "the object survived the unpairing"
    );
    assert!(daemon.scanners.ids().await.is_empty());
    assert!(!daemon.store.contains(&found.id));

    daemon.shutdown().await;
}

/// §3 documents no option for `Pair()`, so one is a client expecting behaviour this
/// daemon does not have — the same stance `StartDiscovery` takes on its filters.
#[tokio::test]
async fn an_unknown_pair_option_is_invalid_args() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("an_unknown_pair_option_is_invalid_args");
    };

    let found = scanner_info("usb:001:002", "Brother MFC-L2710DW");
    let backend = Arc::new(MockBackend::with_scanners([found.clone()]).with_id("mock"));
    let daemon = Daemon::start(&bus, backends(Arc::clone(&backend) as _)).await;

    let client = bus.connect().await;
    discover(&client, &found.id).await;
    let scanner = scanner_proxy(&client, &found.id).await;

    let options = HashMap::from([(
        "pin".to_owned(),
        zbus::zvariant::OwnedValue::try_from(zbus::zvariant::Value::from("123456")).unwrap(),
    )]);
    let error = scanner
        .pair(options)
        .await
        .expect_err("an unknown option must be refused");
    assert_eq!(error_name(&error), "org.freedesktop.DBus.Error.InvalidArgs");
    assert!(error.to_string().contains("pin"), "{error}");

    // Refused before anything started.
    assert_eq!(scanner.pairing_state().await.unwrap(), "none");

    daemon.shutdown().await;
}

/// `CancelPairing()` with nothing in progress succeeds: a client that lost the race with
/// a transition should not have to tell that apart from a real failure.
#[tokio::test]
async fn cancelling_nothing_is_not_an_error() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("cancelling_nothing_is_not_an_error");
    };

    let found = scanner_info("usb:001:002", "Brother MFC-L2710DW");
    let backend = GatedBackend::new([found.clone()]);
    backend.release();
    let daemon = Daemon::start(&bus, backends(Arc::clone(&backend) as _)).await;

    let client = bus.connect().await;
    discover(&client, &found.id).await;
    let scanner = scanner_proxy(&client, &found.id).await;

    scanner.cancel_pairing().await.unwrap();
    assert_eq!(scanner.pairing_state().await.unwrap(), "none");
    assert_eq!(backend.entries(), 0);

    // And after a pairing has finished, too.
    let mut signals = changes(&client, &found.id).await;
    scanner.pair(HashMap::new()).await.unwrap();
    loop {
        if next_pairing_state(&mut signals, SIGNAL_TIMEOUT).await == "done" {
            break;
        }
    }
    backend.hold();
    scanner.cancel_pairing().await.unwrap();
    assert_eq!(scanner.pairing_state().await.unwrap(), "done");
    assert!(scanner.paired().await.unwrap());

    daemon.shutdown().await;
}
