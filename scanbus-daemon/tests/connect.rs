//! `Connect()`, `Disconnect()`, and who owns the listener task, on a real bus.
//!
//! The acceptance criteria of 2.4, run rather than described. Two of them read "produces
//! a job", which is [2.6]'s object and does not exist yet; what stands in for it is the
//! [`ButtonEventSink`] the listener hands presses to, which is the same seam 2.6 will
//! implement. A press that reaches the sink is a press that would have become a `Job1`.
//!
//! Nothing here sleeps waiting for a scan. The restarts are driven by a
//! [`RestartPolicy`] short enough to observe — [`TEST_RESTART`] — and the presses are
//! made by [`MockHandle`](scanbus_core::backend::mock::MockHandle), which is synchronous:
//! "press key 2 now" is a statement between two `await`s.
//!
//! [2.6]: https://github.com/jeanparpaillon/scanbus_service/issues/10

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt as _;
use futures_util::stream::BoxStream;
use scanbus_core::backend::mock::{MockBackend, MockCall, MockError, MockHandle};
use scanbus_core::{
    BackendError, ButtonPressedEvent, Capabilities, PairingProgress, ProfileKind, RawPage,
    ScannerBackend, ScannerId, ScannerInfo, Status, Value,
};
use scanbus_daemon::backends::RankedBackend;
use scanbus_daemon::dbus::{self, BUS_NAME, Manager1, ObjectRegistry, path};
use scanbus_daemon::listeners::{ButtonEvent, ButtonEventSink, RestartPolicy};
use scanbus_daemon::{Backends, Discovery, MemoryPairingStore, ScannerRegistry};
use tokio::sync::mpsc;
use zbus::fdo::{PropertiesChangedStream, PropertiesProxy};
use zbus::zvariant::{OwnedValue, Value as ZValue};

mod common;

use common::{PrivateBus, skipped};

/// How long a signal or a press that should already be on its way is waited for.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(5);

/// The restart budget the tests run with: four attempts, 550 ms in total.
///
/// The daemon's own [`RestartPolicy::DEFAULT`] spends about 45 s before giving up, which
/// is right for a `brscan-skey` being restarted by a package upgrade and wrong for a test
/// suite. Shortening it here is the whole reason
/// [`ScannerRegistry::with_listeners`] takes a policy.
const TEST_RESTART: RestartPolicy = RestartPolicy {
    initial: Duration::from_millis(50),
    max: Duration::from_millis(200),
    attempts: 4,
};

/// How long the exhausted case is given: the budget, plus room for the calls in between.
const GIVE_UP_TIMEOUT: Duration = Duration::from_secs(10);

/// A backend that can be made to refuse `start_listening`, permanently.
///
/// [`MockBackend`] fails discovery and installs on demand but always accepts a listener,
/// and "the vendor daemon is gone for good" is exactly the case the backoff exists for.
/// Everything else is delegated, so the mock's call log and its press handle still work.
struct TestBackend {
    inner: MockBackend,
    refuse_listening: AtomicBool,
}

impl TestBackend {
    fn new(scanners: impl IntoIterator<Item = ScannerInfo>) -> Arc<Self> {
        Arc::new(Self {
            inner: MockBackend::with_scanners(scanners),
            refuse_listening: AtomicBool::new(false),
        })
    }

    fn handle(&self) -> MockHandle {
        self.inner.handle()
    }

    /// From now on, every `start_listening` fails — the device is gone for good.
    fn refuse_listening(&self) {
        self.refuse_listening.store(true, Ordering::SeqCst);
    }

    /// How many listeners have been started since the call log was last cleared.
    fn listens(&self) -> usize {
        self.handle()
            .calls()
            .iter()
            .filter(|call| matches!(call, MockCall::StartListening(_)))
            .count()
    }

    /// Whether the backend was told to stand down since the log was last cleared.
    fn was_stopped(&self) -> bool {
        self.handle()
            .calls()
            .iter()
            .any(|call| matches!(call, MockCall::StopListening { .. }))
    }
}

#[async_trait]
impl ScannerBackend for TestBackend {
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
        self.inner.ensure_installed(scanner, progress).await
    }

    async fn start_listening(
        &self,
        scanner: &ScannerInfo,
    ) -> Result<BoxStream<'static, ButtonPressedEvent>, BackendError> {
        if self.refuse_listening.load(Ordering::SeqCst) {
            return Err(BackendError::NotReachable {
                scanner: scanner.id.clone(),
                detail: "the vendor daemon is not running".to_owned(),
            });
        }
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
        options: &BTreeMap<String, Value>,
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

/// The sink 2.6 replaces: every press, in order, on a channel a test can read.
struct ChannelSink(mpsc::UnboundedSender<ButtonEvent>);

#[async_trait]
impl ButtonEventSink for ChannelSink {
    async fn button_pressed(&self, event: ButtonEvent) {
        // A closed receiver means the test has finished; the listener carries on.
        let _ = self.0.send(event);
    }
}

/// `org.scanbus.Scanner1` as §3 defines it, from the client side.
///
/// Property caching is turned off where this proxy is built ([`scanner_proxy`]): every
/// assertion here is about what the daemon serves *now*, and a cached read would be
/// asserting on how fast the proxy processed a signal.
#[zbus::proxy(interface = "org.scanbus.Scanner1", default_service = "org.scanbus")]
trait Scanner {
    fn pair(&self, options: HashMap<String, OwnedValue>) -> zbus::Result<()>;
    fn unpair(&self) -> zbus::Result<()>;
    fn connect(&self, options: HashMap<String, OwnedValue>) -> zbus::Result<()>;
    fn disconnect(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn connected(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn paired(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn status(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_default_profile(&self, value: &str) -> zbus::Result<()>;
}

/// The service side: what `main.rs` wires up, with a backend and a sink a test drives.
struct Daemon {
    scanners: Arc<ScannerRegistry>,
    discovery: Arc<Discovery>,
    objects: Arc<ObjectRegistry>,
    backend: Arc<TestBackend>,
    events: mpsc::UnboundedReceiver<ButtonEvent>,
}

impl Daemon {
    async fn start(bus: &PrivateBus, scanners: impl IntoIterator<Item = ScannerInfo>) -> Self {
        let connection = bus.connect().await;
        let objects = Arc::new(ObjectRegistry::new(connection.clone()).await.unwrap());
        let backend = TestBackend::new(scanners);
        let (sender, events) = mpsc::unbounded_channel();

        let registry = ScannerRegistry::with_listeners(
            Arc::clone(&objects),
            Arc::new(MemoryPairingStore::new()),
            Arc::new(ChannelSink(sender)),
            TEST_RESTART,
        );
        let discovery = Arc::new(Discovery::new(
            Backends::new([Arc::clone(&backend) as Arc<dyn ScannerBackend>]),
            Arc::clone(&registry),
        ));

        objects
            .add(path::manager(), Manager1::new(Arc::clone(&discovery)))
            .await
            .unwrap();
        dbus::request_name(&connection).await.unwrap();

        Self {
            scanners: registry,
            discovery,
            objects,
            backend,
            events,
        }
    }

    /// Publishes `info` as a paired scanner, the way 4.2's restore path will.
    ///
    /// Deliberately *not* through `Pair()`: it publishes an object that is paired and not
    /// yet listening, which is the state `Connect()` is defined against. The pairing path
    /// has its own test below.
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

    fn press(&self, id: &ScannerId, button: u32) -> Result<(), MockError> {
        self.backend.handle().press_button(id, button)
    }

    /// The next press to reach the sink, or a failed test.
    async fn next_event(&mut self) -> ButtonEvent {
        tokio::time::timeout(SIGNAL_TIMEOUT, self.events.recv())
            .await
            .expect("no button event within the timeout")
            .expect("the sink's channel was closed")
    }

    /// Presses `button` as soon as there is a listener to receive it.
    ///
    /// The retry is the point: after a stream ends, the restart is a backoff away, and
    /// the mock refuses a press until it has landed. Bounded, so a listener that never
    /// comes back fails the test instead of hanging it.
    async fn press_when_listening(&self, id: &ScannerId, button: u32) {
        let deadline = tokio::time::Instant::now() + GIVE_UP_TIMEOUT;

        loop {
            match self.press(id, button) {
                Ok(()) => return,
                Err(error) if tokio::time::Instant::now() >= deadline => {
                    panic!("the listener never came back: {error}")
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    }

    async fn shutdown(&self) {
        self.discovery.stop().await;
        self.scanners.shutdown().await;
        self.objects.shutdown().await;
    }
}

fn scanner_info(address: &str, status: Status) -> ScannerInfo {
    ScannerInfo {
        id: ScannerId::from_backend("mock", address).unwrap(),
        name: "Brother MFC-L2710DW".to_owned(),
        backend: "proprietary:brother".to_owned(),
        address: address.to_owned(),
        capabilities: Capabilities::default(),
        status,
    }
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

/// `{"profile": "<name>"}`, the one `Connect()` option of §3.
fn profile_option(name: &str) -> HashMap<String, OwnedValue> {
    HashMap::from([(
        "profile".to_owned(),
        OwnedValue::try_from(ZValue::from(name)).unwrap(),
    )])
}

/// The raw `PropertiesChanged` stream for a scanner object.
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

/// The next value `property` is announced with, skipping signals that do not mention it.
async fn next_change(
    stream: &mut PropertiesChangedStream,
    property: &str,
    timeout: Duration,
) -> String {
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let signal = tokio::time::timeout_at(deadline, stream.next())
            .await
            .unwrap_or_else(|_| panic!("no PropertiesChanged for {property} within the timeout"))
            .expect("the signal stream ended");
        let args = signal.args().unwrap();
        assert_eq!(args.interface_name, "org.scanbus.Scanner1");

        let Some(value) = args.changed_properties.get(property) else {
            continue;
        };

        return match value {
            zbus::zvariant::Value::Bool(flag) => flag.to_string(),
            zbus::zvariant::Value::Str(text) => text.to_string(),
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

/// Acceptance: `Connect` then a press produces a job; `Disconnect` then a press produces
/// nothing, and the backend is told to stand down.
#[tokio::test]
async fn connect_delivers_presses_and_disconnect_stops_them() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("connect_delivers_presses_and_disconnect_stops_them");
    };
    let info = scanner_info("usb:001:002", Status::Online);
    let mut daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.restore(&info).await;

    let scanner = scanner_proxy(&client, &info.id).await;
    assert!(scanner.paired().await.unwrap());
    assert!(
        !scanner.connected().await.unwrap(),
        "restoring a pairing publishes the object; 4.2 is what connects it"
    );
    // Nothing is listening, so a press has nowhere to go.
    assert_eq!(
        daemon.press(&info.id, 2),
        Err(MockError::NotListening(info.id.clone()))
    );

    scanner.connect(HashMap::new()).await.unwrap();
    assert!(scanner.connected().await.unwrap());

    daemon.press(&info.id, 2).unwrap();
    let event = daemon.next_event().await;
    assert_eq!(event.scanner(), &info.id);
    assert_eq!(event.button_index(), 2);
    // Nothing configured anywhere: §4's `Profile=""`, i.e. deliver it raw.
    assert_eq!(event.profile(None), None);

    daemon.backend.handle().clear_calls();
    scanner.disconnect().await.unwrap();

    assert!(!scanner.connected().await.unwrap());
    assert!(
        daemon.backend.was_stopped(),
        "Disconnect() has to reach the backend: {:?}",
        daemon.backend.handle().calls()
    );
    assert_eq!(
        daemon.press(&info.id, 2),
        Err(MockError::NotListening(info.id.clone()))
    );
    assert!(daemon.events.try_recv().is_err(), "a press was delivered");

    daemon.shutdown().await;
}

/// Acceptance: calling `Connect` twice starts one listener task.
#[tokio::test]
async fn connecting_twice_starts_one_listener() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("connecting_twice_starts_one_listener");
    };
    let info = scanner_info("usb:001:002", Status::Online);
    let mut daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.restore(&info).await;

    let scanner = scanner_proxy(&client, &info.id).await;
    daemon.backend.handle().clear_calls();

    scanner.connect(HashMap::new()).await.unwrap();
    scanner.connect(HashMap::new()).await.unwrap();
    scanner.connect(HashMap::new()).await.unwrap();

    assert_eq!(
        daemon.backend.listens(),
        1,
        "{:?}",
        daemon.backend.handle().calls()
    );
    assert!(scanner.connected().await.unwrap());

    // And one listener is what delivers: three presses, three events, in order.
    for button in 0..3 {
        daemon.press(&info.id, button).unwrap();
    }
    for button in 0..3 {
        assert_eq!(daemon.next_event().await.button_index(), button);
    }

    daemon.shutdown().await;
}

/// Acceptance: killing the backend's stream restarts the listener, and a press after the
/// restart still produces a job.
#[tokio::test]
async fn a_stream_that_ends_is_restarted() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_stream_that_ends_is_restarted");
    };
    let info = scanner_info("usb:001:002", Status::Online);
    let mut daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.restore(&info).await;

    let scanner = scanner_proxy(&client, &info.id).await;
    scanner.connect(HashMap::new()).await.unwrap();
    daemon.press(&info.id, 1).unwrap();
    assert_eq!(daemon.next_event().await.button_index(), 1);

    // The device goes away as far as the backend is concerned: the stream simply ends.
    assert!(daemon.backend.handle().end_listener(&info.id));

    daemon.press_when_listening(&info.id, 3).await;
    assert_eq!(daemon.next_event().await.button_index(), 3);
    assert!(
        scanner.connected().await.unwrap(),
        "a stream that came back is not a disconnection"
    );
    assert_eq!(scanner.status().await.unwrap(), "online");

    daemon.shutdown().await;
}

/// Acceptance: with the stream made to fail permanently, `Status` becomes `"error"` and
/// `Connected` false within the backoff budget.
#[tokio::test]
async fn a_listener_that_cannot_be_restored_ends_in_error() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_listener_that_cannot_be_restored_ends_in_error");
    };
    let info = scanner_info("usb:001:002", Status::Online);
    let daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.restore(&info).await;

    let scanner = scanner_proxy(&client, &info.id).await;
    scanner.connect(HashMap::new()).await.unwrap();

    let mut announced = changes(&client, &info.id).await;
    daemon.backend.refuse_listening();
    assert!(daemon.backend.handle().end_listener(&info.id));

    // `Connected` is announced after `Status`, so waiting for it is waiting for both.
    assert_eq!(
        next_change(&mut announced, "Connected", GIVE_UP_TIMEOUT).await,
        "false"
    );
    assert_eq!(scanner.status().await.unwrap(), "error");
    assert!(!scanner.connected().await.unwrap());
    // The pairing is untouched: §9's "reachable ≠ paired" holds here too.
    assert!(scanner.paired().await.unwrap());
    assert!(!daemon.scanners.is_listening(&info.id).await);

    daemon.shutdown().await;
}

/// Acceptance: `Connect` with `{"profile":"document"}` yields an event whose profile is
/// `document` — and the precedence around it holds.
#[tokio::test]
async fn the_session_profile_ranks_between_the_button_and_the_default() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("the_session_profile_ranks_between_the_button_and_the_default");
    };
    let info = scanner_info("usb:001:002", Status::Online);
    let mut daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.restore(&info).await;

    let scanner = scanner_proxy(&client, &info.id).await;
    scanner.set_default_profile("image").await.unwrap();
    scanner.connect(profile_option("document")).await.unwrap();

    daemon.press(&info.id, 1).unwrap();
    let event = daemon.next_event().await;

    assert_eq!(event.session_profile, Some(ProfileKind::Document));
    assert_eq!(event.default_profile, Some(ProfileKind::Image));
    // The button's own key is empty here, so the session profile is what a job gets…
    assert_eq!(event.profile(None), Some(ProfileKind::Document));
    // …and a button that has one still outranks it (2.5 is what will set it).
    assert_eq!(
        event.profile(Some(ProfileKind::Ocr)),
        Some(ProfileKind::Ocr)
    );

    // Disconnecting forgets it: the session profile belongs to the connection.
    scanner.disconnect().await.unwrap();
    scanner.connect(HashMap::new()).await.unwrap();
    daemon.press(&info.id, 1).unwrap();
    let event = daemon.next_event().await;

    assert_eq!(event.session_profile, None);
    assert_eq!(event.profile(None), Some(ProfileKind::Image));

    daemon.shutdown().await;
}

/// Acceptance: `Connect` on an offline scanner is `NotReachable`, on an unpaired one
/// `NotPaired`.
#[tokio::test]
async fn connect_refuses_an_offline_or_unpaired_scanner() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("connect_refuses_an_offline_or_unpaired_scanner");
    };
    let offline = scanner_info("usb:001:002", Status::Offline);
    let unpaired = scanner_info("usb:001:003", Status::Online);
    let daemon = Daemon::start(&bus, []).await;
    let client = bus.connect().await;

    daemon.restore(&offline).await;
    daemon.sight(&unpaired).await;

    let error = scanner_proxy(&client, &offline.id)
        .await
        .connect(HashMap::new())
        .await
        .expect_err("§3: Connect fails when Status is offline");
    assert_eq!(error_name(&error), "org.scanbus.Error.NotReachable");

    let scanner = scanner_proxy(&client, &unpaired.id).await;
    assert!(!scanner.paired().await.unwrap());
    let error = scanner
        .connect(HashMap::new())
        .await
        .expect_err("a scanner with no association has no keys to listen for");
    assert_eq!(error_name(&error), "org.scanbus.Error.NotPaired");

    // Neither refusal armed anything.
    assert_eq!(daemon.backend.listens(), 0);
    assert!(!scanner.connected().await.unwrap());

    daemon.shutdown().await;
}

/// Checklist: the `options` map is validated, and a refused `Connect` changes nothing.
#[tokio::test]
async fn connect_refuses_options_it_cannot_honour() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("connect_refuses_options_it_cannot_honour");
    };
    let info = scanner_info("usb:001:002", Status::Online);
    let mut daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.restore(&info).await;

    let scanner = scanner_proxy(&client, &info.id).await;

    let error = scanner
        .connect(HashMap::from([(
            "quality".to_owned(),
            OwnedValue::try_from(ZValue::from("best")).unwrap(),
        )]))
        .await
        .expect_err("an option this daemon does not implement must not be ignored");
    assert_eq!(error_name(&error), "org.freedesktop.DBus.Error.InvalidArgs");

    // `ocr` parses — a config UI can hold one — and this pipeline will not run it.
    let error = scanner
        .connect(profile_option("ocr"))
        .await
        .expect_err("ocr is outside SupportedProfiles");
    assert_eq!(error_name(&error), "org.scanbus.Error.UnsupportedProfile");

    let error = scanner
        .connect(profile_option("pdf"))
        .await
        .expect_err("pdf is not a profile name at all");
    assert_eq!(error_name(&error), "org.scanbus.Error.UnsupportedProfile");

    let error = scanner
        .connect(HashMap::from([(
            "profile".to_owned(),
            OwnedValue::try_from(ZValue::from(42u32)).unwrap(),
        )]))
        .await
        .expect_err("the profile option is a string");
    assert_eq!(error_name(&error), "org.freedesktop.DBus.Error.InvalidArgs");

    // Nothing was connected, and no session profile was left behind: the next call
    // succeeds with the profile *it* asked for, and not with a residue of the four above.
    assert!(!scanner.connected().await.unwrap());
    assert_eq!(daemon.backend.listens(), 0);

    scanner.connect(HashMap::new()).await.unwrap();
    daemon.press(&info.id, 0).unwrap();
    assert_eq!(daemon.next_event().await.session_profile, None);

    daemon.shutdown().await;
}

/// Checklist: pairing asks for the same listener `Connect()` does, so a client that pairs
/// and walks up to the device does not have to connect first.
///
/// Also the `Connected`-with-`PropertiesChanged` criterion: the value is learnt from the
/// signal, not read back off a property.
#[tokio::test]
async fn pairing_starts_the_listener_without_a_connect() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("pairing_starts_the_listener_without_a_connect");
    };
    let info = scanner_info("usb:001:002", Status::Online);
    let mut daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.sight(&info).await;

    let scanner = scanner_proxy(&client, &info.id).await;
    let mut announced = changes(&client, &info.id).await;

    scanner.pair(HashMap::new()).await.unwrap();

    assert_eq!(
        next_change(&mut announced, "Connected", SIGNAL_TIMEOUT).await,
        "true"
    );
    assert!(scanner.paired().await.unwrap());

    daemon.press(&info.id, 3).unwrap();
    assert_eq!(daemon.next_event().await.button_index(), 3);

    // And `Unpair()` takes it away again, before the association goes.
    scanner.unpair().await.unwrap();
    assert_eq!(
        daemon.press(&info.id, 3),
        Err(MockError::NotListening(info.id.clone()))
    );
    assert!(!daemon.scanners.is_listening(&info.id).await);

    daemon.shutdown().await;
}

/// Checklist: shutdown cancels the listeners, before the object tree goes.
#[tokio::test]
async fn shutdown_stops_every_listener() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("shutdown_stops_every_listener");
    };
    let first = scanner_info("usb:001:002", Status::Online);
    let second = scanner_info("usb:001:003", Status::Online);
    let daemon = Daemon::start(&bus, []).await;
    let client = bus.connect().await;

    for info in [&first, &second] {
        daemon.restore(info).await;
        scanner_proxy(&client, &info.id)
            .await
            .connect(HashMap::new())
            .await
            .unwrap();
    }
    assert_eq!(daemon.backend.listens(), 2);

    daemon.backend.handle().clear_calls();
    daemon.scanners.shutdown().await;

    for info in [&first, &second] {
        assert!(!daemon.scanners.is_listening(&info.id).await);
        assert_eq!(
            daemon.press(&info.id, 0),
            Err(MockError::NotListening(info.id.clone()))
        );
    }
    assert!(daemon.backend.was_stopped());

    daemon.shutdown().await;
}
