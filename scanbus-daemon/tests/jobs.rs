//! `Job1`, on a real bus: the acceptance criteria of 2.6, run rather than described.
//!
//! Every one of them is about what a client *sees while a scan is happening*, which is
//! the thing a batch of pages cannot demonstrate: the mock's batch stream is ready on
//! every poll, so a three-page scan is over before anything can read `PageCount`. So the
//! pages are delivered through [`MockHandle::open_pages`], whose
//! [`PageFeed`](scanbus_core::backend::mock::PageFeed) is synchronous — "page 2 arrives
//! now" is a statement between two `await`s — and the job stays in `"receiving"` for
//! exactly as long as the test holds the feed.
//!
//! Nothing sleeps waiting for a scan. The one place a timer is unavoidable is the
//! retention window, and that is what [`JobRegistry::with_retention`] is for: the suite
//! runs with [`TEST_RETENTION`] rather than the daemon's minute.
//!
//! The job ids are predictable on purpose — [`JobRegistry::FIRST_ID`] and up, minted in
//! order — because the feed has to be registered under the id the daemon will ask the
//! backend for, before the key is pressed. That the ids really are allocated that way is
//! itself one of the assertions below.

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageBuffer, Rgba};
use scanbus_core::backend::mock::{MockBackend, MockHandle, PageFeed};
use scanbus_core::{
    BackendError, ButtonsCapability, Capabilities, PageFormat, RawPage, ScannerBackend, ScannerId,
    ScannerInfo, Status,
};
use scanbus_daemon::dbus::{self, BUS_NAME, Manager1, ObjectRegistry, Profile1, path};
use scanbus_daemon::{
    Backends, Discovery, JobRegistry, JobTrigger, MemoryPairingStore, ProfileRegistry,
    RestartPolicy, ScannerRegistry,
};
use zbus::fdo::{ObjectManagerProxy, PropertiesChangedStream, PropertiesProxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value as ZValue};

mod common;

use common::{PrivateBus, skipped};

/// How long a signal that should already be on its way is waited for.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(5);

/// The retention window the suite runs with.
///
/// [`JobRegistry::RETENTION`] is a minute, which is right for a client that wants to read
/// `Result` off a job it saw finish and wrong for a test suite. Long enough here that the
/// assertions *before* the window are not racing it, short enough that waiting it out is
/// not felt.
const TEST_RETENTION: Duration = Duration::from_millis(300);

/// `org.scanbus.Job1` as §4 defines it, from the client side.
///
/// Written out rather than taken from `scanbus-client` for the same reason the other
/// suites do: property caching is turned off where this proxy is built, so every
/// assertion is about what the daemon serves *now* and not about how fast the proxy
/// processed a signal.
#[zbus::proxy(interface = "org.scanbus.Job1", default_service = "org.scanbus")]
trait Job {
    #[zbus(property)]
    fn scanner(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn button(&self) -> zbus::Result<i32>;
    #[zbus(property)]
    fn profile(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn page_count(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn result(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
    #[zbus(property)]
    fn error(&self) -> zbus::Result<String>;
}

/// Just enough of `org.scanbus.Button1` to reassign a key mid-scan.
#[zbus::proxy(interface = "org.scanbus.Button1", default_service = "org.scanbus")]
trait Button {
    #[zbus(property)]
    fn profile(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_profile(&self, value: &str) -> zbus::Result<()>;
}

/// Just enough of `org.scanbus.Profile1` to override image defaults, and to ask where a
/// scan with no `output_folder` would land.
#[zbus::proxy(interface = "org.scanbus.Profile1", default_service = "org.scanbus")]
trait Profile {
    #[zbus(property)]
    fn set_options(&self, value: HashMap<String, OwnedValue>) -> zbus::Result<()>;
    #[zbus(property)]
    fn options_schema(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
}

/// Just enough of `org.scanbus.Scanner1` to connect and to set the fallback profile.
#[zbus::proxy(interface = "org.scanbus.Scanner1", default_service = "org.scanbus")]
trait Scanner {
    fn connect(&self, options: HashMap<String, OwnedValue>) -> zbus::Result<()>;
    fn disconnect(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn set_default_profile(&self, value: &str) -> zbus::Result<()>;
}

/// The service side, wired the way `main.rs` wires it, with the retention window shrunk.
struct Daemon {
    scanners: Arc<ScannerRegistry>,
    discovery: Arc<Discovery>,
    objects: Arc<ObjectRegistry>,
    backend: Arc<MockBackend>,
}

impl Daemon {
    async fn start(bus: &PrivateBus, scanners: impl IntoIterator<Item = ScannerInfo>) -> Self {
        Self::start_writing_under(bus, scanners, None).await
    }

    /// The same daemon, with the XDG user directories replaced by `output_root`.
    ///
    /// The one test that scans *without* an `output_folder` needs this: it is asserting
    /// about where the daemon writes when nothing tells it to, and the honest answer on a
    /// developer's machine is `~/Pictures`.
    async fn start_writing_under(
        bus: &PrivateBus,
        scanners: impl IntoIterator<Item = ScannerInfo>,
        output_root: Option<std::path::PathBuf>,
    ) -> Self {
        let connection = bus.connect().await;
        let objects = Arc::new(ObjectRegistry::new(connection.clone()).await.unwrap());
        let backend = Arc::new(MockBackend::with_scanners(scanners));
        let profiles = Arc::new(match output_root {
            Some(root) => ProfileRegistry::ephemeral_with_output_root(root),
            None => ProfileRegistry::ephemeral(),
        });

        // The real sink, with the real lifecycle — only the window is a test's. Anything
        // else here would be testing a stand-in.
        let jobs = Arc::new(JobRegistry::with_retention(
            Arc::clone(&objects),
            Arc::clone(&profiles),
            TEST_RETENTION,
        ));
        let registry = ScannerRegistry::with_listeners(
            Arc::clone(&objects),
            Arc::new(MemoryPairingStore::new()),
            jobs,
            RestartPolicy::DEFAULT,
        );
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
        dbus::request_name(&connection).await.unwrap();

        Self {
            scanners: registry,
            discovery,
            objects,
            backend,
        }
    }

    fn handle(&self) -> MockHandle {
        self.backend.handle()
    }

    /// Publishes `info` as a paired scanner and starts listening — the state a walk-up
    /// scan happens in.
    async fn ready(&self, info: &ScannerInfo) {
        self.scanners
            .register_persistent(
                Arc::clone(&self.backend) as Arc<dyn ScannerBackend>,
                info.clone(),
            )
            .await
            .unwrap();
        self.scanners.ensure_listening(&info.id).await.unwrap();
    }

    fn press(&self, id: &ScannerId, button: u32) {
        self.handle().press_button(id, button).unwrap();
    }

    async fn shutdown(&self) {
        self.discovery.stop().await;
        self.scanners.shutdown().await;
        self.objects.shutdown().await;
    }
}

/// The four-key Brother of §5, reachable and with an ADF.
fn brother() -> ScannerInfo {
    ScannerInfo {
        id: ScannerId::from_backend("mock", "usb:001:002").unwrap(),
        name: "Brother MFC-L2710DW".to_owned(),
        backend: "proprietary:brother".to_owned(),
        address: "usb:001:002".to_owned(),
        capabilities: Capabilities {
            buttons: ButtonsCapability {
                count: 4,
                label_configurable: false,
            },
            ..Capabilities::default()
        },
        status: Status::Online,
    }
}

fn page(index: u32) -> RawPage {
    RawPage {
        index,
        format: PageFormat::Pnm,
        resolution_dpi: 300,
        // Minimal valid PGM (P5) payload: 1x1 pixel, value 0x42.
        data: b"P5\n1 1\n255\n\x42".to_vec(),
    }
}

fn jpeg_page(index: u32, fill: u8) -> RawPage {
    let pixels: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_pixel(
        4,
        4,
        Rgba([fill, fill.saturating_add(1), fill.saturating_add(2), 255]),
    );
    let mut data = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut data, 80);
    encoder
        .encode_image(&DynamicImage::ImageRgba8(pixels))
        .unwrap();

    RawPage {
        index,
        format: PageFormat::Jpeg,
        resolution_dpi: 300,
        data,
    }
}

/// The feed the daemon's *next* job will read, opened under the id it will mint.
fn open_next_job(handle: &MockHandle, job_id: u64) -> PageFeed {
    handle.open_pages(format!("job-{job_id}"))
}

async fn job_proxy(
    connection: &zbus::Connection,
    id: &ScannerId,
    job_id: u64,
) -> JobProxy<'static> {
    JobProxy::builder(connection)
        .path(path::job(id, job_id))
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap()
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

async fn profile_proxy(
    connection: &zbus::Connection,
    profile: scanbus_core::ProfileKind,
) -> ProfileProxy<'static> {
    ProfileProxy::builder(connection)
        .path(path::profile(profile))
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

async fn manager(connection: &zbus::Connection) -> ObjectManagerProxy<'static> {
    ObjectManagerProxy::builder(connection)
        .destination(BUS_NAME)
        .unwrap()
        .path(path::manager())
        .unwrap()
        .build()
        .await
        .unwrap()
}

/// Every `Job1` object the manager reports, in tree order.
async fn job_paths(connection: &zbus::Connection) -> Vec<OwnedObjectPath> {
    let mut paths: Vec<OwnedObjectPath> = manager(connection)
        .await
        .get_managed_objects()
        .await
        .unwrap()
        .into_iter()
        .filter(|(_, interfaces)| interfaces.contains_key("org.scanbus.Job1"))
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
async fn next_change(stream: &mut PropertiesChangedStream, property: &str) -> OwnedValue {
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

async fn next_string(stream: &mut PropertiesChangedStream, property: &str) -> String {
    String::try_from(next_change(stream, property).await)
        .unwrap_or_else(|_| panic!("{property} did not arrive as a string"))
}

async fn next_u32(stream: &mut PropertiesChangedStream, property: &str) -> u32 {
    u32::try_from(next_change(stream, property).await)
        .unwrap_or_else(|_| panic!("{property} did not arrive as a u32"))
}

/// Waits for a job object to appear, and answers with its path.
///
/// The press returns before the daemon has fetched the pages, so a test that read
/// `GetManagedObjects` straight afterwards would be racing the first page rather than
/// asserting on it.
async fn await_job(connection: &zbus::Connection, id: &ScannerId, job_id: u64) -> OwnedObjectPath {
    let wanted = path::job(id, job_id);
    let mut added = manager(connection)
        .await
        .receive_interfaces_added()
        .await
        .unwrap();

    // Already there, if the daemon got ahead of the subscription.
    if job_paths(connection).await.contains(&wanted) {
        return wanted;
    }

    let deadline = tokio::time::Instant::now() + SIGNAL_TIMEOUT;
    loop {
        let signal = tokio::time::timeout_at(deadline, added.next())
            .await
            .expect("no InterfacesAdded for the job within the timeout")
            .expect("the signal stream ended");

        if *signal.args().unwrap().object_path() == *wanted {
            return wanted;
        }
    }
}

/// Waits for the job object to go away again.
async fn await_removed(connection: &zbus::Connection, path: &OwnedObjectPath) {
    let mut removed = manager(connection)
        .await
        .receive_interfaces_removed()
        .await
        .unwrap();

    if !job_paths(connection).await.contains(path) {
        return;
    }

    let deadline = tokio::time::Instant::now() + SIGNAL_TIMEOUT;
    loop {
        let signal = tokio::time::timeout_at(deadline, removed.next())
            .await
            .expect("the job object was never removed")
            .expect("the signal stream ended");

        if *signal.args().unwrap().object_path() == **path {
            return;
        }
    }
}

/// Acceptance: a button press publishes a job object carrying `Button` and `Profile`,
/// before any page has been processed — and a client that only subscribed to the manager
/// sees it, which is the whole reason a job is an object (§4).
#[tokio::test]
async fn a_press_publishes_a_job_stamped_with_its_key_and_profile() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_press_publishes_a_job_stamped_with_its_key_and_profile");
    };
    let info = brother();
    let daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.ready(&info).await;

    // Key 2 is the "Scan to OCR" key of §5, assigned `document` by the host.
    button_proxy(&client, &info.id, 2)
        .await
        .set_profile("document")
        .await
        .unwrap();

    let feed = open_next_job(&daemon.handle(), JobRegistry::FIRST_ID);
    daemon.press(&info.id, 2);

    // The first page is what publishes the object (§1), and nothing has been processed:
    // the feed is still open, so the pipeline has not been reached.
    feed.page(page(0)).unwrap();
    let path = await_job(&client, &info.id, JobRegistry::FIRST_ID).await;

    let job = job_proxy(&client, &info.id, JobRegistry::FIRST_ID).await;
    assert_eq!(job.scanner().await.unwrap(), path::scanner(&info.id));
    assert_eq!(job.button().await.unwrap(), 2);
    assert_eq!(job.profile().await.unwrap(), "document");
    assert_eq!(job.state().await.unwrap(), "receiving");
    assert_eq!(job.page_count().await.unwrap(), 1);
    assert_eq!(job.error().await.unwrap(), "");
    assert!(job.result().await.unwrap().is_empty());
    let mut announced = changes(&client, path::job(&info.id, JobRegistry::FIRST_ID)).await;

    // Acceptance: a client that arrives now — mid-scan — finds the job in the manager's
    // dict without having seen the signal that created it.
    let late = bus.connect().await;
    assert_eq!(job_paths(&late).await, vec![path]);

    feed.end();
    assert_eq!(next_string(&mut announced, "State").await, "processing");
    let terminal = next_string(&mut announced, "State").await;
    assert_eq!(
        terminal,
        "done",
        "job terminal state={terminal}, error={}",
        job.error().await.unwrap()
    );
    let result = job.result().await.unwrap();
    let path = String::try_from(result["path"].clone()).unwrap();
    assert!(
        path.ends_with(".pdf"),
        "document profile maps to {{\"path\": s}}"
    );

    daemon.shutdown().await;
}

/// Acceptance: `document` with `multi_page=false` writes one single-page PDF per incoming
/// page, and reports them as `Result.paths`.
#[tokio::test]
async fn document_profile_multi_page_false_reports_paths() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("document_profile_multi_page_false_reports_paths");
    };
    let info = brother();
    let daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.ready(&info).await;

    let out = std::env::temp_dir().join(format!(
        "scanbus-document-multi-false-{}-{}",
        std::process::id(),
        JobRegistry::FIRST_ID
    ));
    fs::create_dir_all(&out).unwrap();

    let document = profile_proxy(&client, scanbus_core::ProfileKind::Document).await;
    document
        .set_options(HashMap::from([
            (
                "format".to_owned(),
                OwnedValue::try_from(ZValue::from("pdf")).unwrap(),
            ),
            (
                "multi_page".to_owned(),
                OwnedValue::try_from(ZValue::from(false)).unwrap(),
            ),
            (
                "output_folder".to_owned(),
                OwnedValue::try_from(ZValue::from(out.to_string_lossy().to_string())).unwrap(),
            ),
        ]))
        .await
        .unwrap();

    let key = button_proxy(&client, &info.id, 2).await;
    key.set_profile("document").await.unwrap();

    let feed = open_next_job(&daemon.handle(), JobRegistry::FIRST_ID);
    daemon.press(&info.id, 2);
    for index in 0..3 {
        feed.page(jpeg_page(index, 20 + (index as u8) * 50))
            .unwrap();
    }
    let path = await_job(&client, &info.id, JobRegistry::FIRST_ID).await;
    let job = job_proxy(&client, &info.id, JobRegistry::FIRST_ID).await;
    let mut announced = changes(&client, path.clone()).await;
    feed.end();

    assert_eq!(next_string(&mut announced, "State").await, "processing");
    assert_eq!(next_string(&mut announced, "State").await, "done");

    let result = job.result().await.unwrap();
    let paths = result
        .get("paths")
        .and_then(|value| Vec::<String>::try_from(value.clone()).ok())
        .unwrap();
    assert_eq!(paths.len(), 3);
    for pdf in &paths {
        assert!(pdf.ends_with(".pdf"));
        assert!(std::path::Path::new(pdf).exists(), "missing output: {pdf}");
    }

    await_removed(&client, &path).await;
    let _ = fs::remove_dir_all(out);
    daemon.shutdown().await;
}

/// Acceptance (10.13): the folder `Profile1.OptionsSchema` reports as the effective
/// default is the folder a scan with no `output_folder` really writes to.
///
/// This is the criterion the property is worth having for. A client shows that path to
/// the user as "where the next scan lands"; if the daemon's own directory logic and the
/// published default were two computations, the GUI would be confidently wrong the first
/// time they diverged. So the scan runs with `output_folder` unset — the state a fresh
/// profile store is in — and the assertion is against the file the job reports.
#[tokio::test]
async fn an_unset_output_folder_lands_where_the_schema_says_it_will() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("an_unset_output_folder_lands_where_the_schema_says_it_will");
    };
    let root = std::env::temp_dir().join(format!(
        "scanbus-schema-output-root-{}-{}",
        std::process::id(),
        JobRegistry::FIRST_ID
    ));
    let info = brother();
    let daemon = Daemon::start_writing_under(&bus, [info.clone()], Some(root.clone())).await;
    let client = bus.connect().await;
    daemon.ready(&info).await;

    // Read first, scan second: the client's rule in §6 is "display the default when the
    // key is absent", and this is that read.
    let schema = profile_proxy(&client, scanbus_core::ProfileKind::Image)
        .await
        .options_schema()
        .await
        .unwrap();
    let folder = HashMap::<String, OwnedValue>::try_from(schema["output_folder"].clone()).unwrap();
    let published = String::try_from(folder["default"].clone()).unwrap();
    assert!(!published.is_empty());

    button_proxy(&client, &info.id, 1)
        .await
        .set_profile("image")
        .await
        .unwrap();

    let feed = open_next_job(&daemon.handle(), JobRegistry::FIRST_ID);
    daemon.press(&info.id, 1);
    feed.page(jpeg_page(0, 40)).unwrap();
    let path = await_job(&client, &info.id, JobRegistry::FIRST_ID).await;
    let job = job_proxy(&client, &info.id, JobRegistry::FIRST_ID).await;
    let mut announced = changes(&client, path.clone()).await;
    feed.end();

    assert_eq!(next_string(&mut announced, "State").await, "processing");
    let terminal = next_string(&mut announced, "State").await;
    assert_eq!(terminal, "done", "error={}", job.error().await.unwrap());

    let result = job.result().await.unwrap();
    let written = Vec::<String>::try_from(result["paths"].clone()).unwrap();
    let parent = std::path::Path::new(&written[0])
        .parent()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert_eq!(
        parent, published,
        "the page landed somewhere the schema did not announce"
    );
    assert!(std::path::Path::new(&written[0]).exists());

    await_removed(&client, &path).await;
    let _ = fs::remove_dir_all(&root);
    daemon.shutdown().await;
}

/// Acceptance: feeding three pages moves `PageCount` 1→2→3 with `State="receiving"`, then
/// `processing` once the stream ends — §9's rule for an ADF batch.
#[tokio::test]
async fn three_pages_count_up_and_then_the_capture_ends() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("three_pages_count_up_and_then_the_capture_ends");
    };
    let info = brother();
    let daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.ready(&info).await;

    let feed = open_next_job(&daemon.handle(), JobRegistry::FIRST_ID);
    daemon.press(&info.id, 0);

    feed.page(page(0)).unwrap();
    let path = await_job(&client, &info.id, JobRegistry::FIRST_ID).await;
    let job = job_proxy(&client, &info.id, JobRegistry::FIRST_ID).await;
    let mut announced = changes(&client, path.clone()).await;

    assert_eq!(job.page_count().await.unwrap(), 1);
    assert_eq!(job.state().await.unwrap(), "receiving");

    // Sheet two and sheet three, each announced, each still mid-capture.
    for expected in [2u32, 3] {
        feed.page(page(expected - 1)).unwrap();
        assert_eq!(next_u32(&mut announced, "PageCount").await, expected);
        assert_eq!(
            job.state().await.unwrap(),
            "receiving",
            "a job stays receiving while the backend reports further pages (§9)"
        );
    }

    // The last sheet: the stream ends, which is what marks the end of capture.
    feed.end();
    assert_eq!(next_string(&mut announced, "State").await, "processing");
    // And then the profile pipeline result.
    assert_eq!(next_string(&mut announced, "State").await, "done");

    assert_eq!(job.page_count().await.unwrap(), 3);
    assert_eq!(job.error().await.unwrap(), "");
    assert!(
        job.result().await.unwrap().is_empty(),
        "this run has no profile assignment, so Result stays empty"
    );

    daemon.shutdown().await;
}

/// Acceptance: changing `Button1.Profile` while the job is in `receiving` does not change
/// `Job1.Profile` — §4's "copied at trigger time", which is the reason the copy exists.
#[tokio::test]
async fn reassigning_the_key_mid_scan_does_not_move_the_job() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("reassigning_the_key_mid_scan_does_not_move_the_job");
    };
    let info = brother();
    let daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.ready(&info).await;

    let key = button_proxy(&client, &info.id, 2).await;
    key.set_profile("document").await.unwrap();

    let feed = open_next_job(&daemon.handle(), JobRegistry::FIRST_ID);
    daemon.press(&info.id, 2);
    feed.page(page(0)).unwrap();
    await_job(&client, &info.id, JobRegistry::FIRST_ID).await;

    let job = job_proxy(&client, &info.id, JobRegistry::FIRST_ID).await;
    assert_eq!(job.state().await.unwrap(), "receiving");
    assert_eq!(job.profile().await.unwrap(), "document");

    // The user reassigns the key while the batch is still feeding.
    key.set_profile("image").await.unwrap();
    assert_eq!(key.profile().await.unwrap(), "image");

    assert_eq!(
        job.profile().await.unwrap(),
        "document",
        "the pages already received keep the profile the trigger stamped"
    );

    // And the *next* press picks up the new assignment, which is the other half of it.
    let next = JobRegistry::FIRST_ID + 1;
    let second = open_next_job(&daemon.handle(), next);
    feed.end();
    daemon.press(&info.id, 2);
    second.page(page(0)).unwrap();
    await_job(&client, &info.id, next).await;

    assert_eq!(
        job_proxy(&client, &info.id, next)
            .await
            .profile()
            .await
            .unwrap(),
        "image"
    );

    second.end();
    daemon.shutdown().await;
}

/// Acceptance: a failing page stream yields `State="error"` with `Error` set, and the
/// object is removed after the retention window — no registered object left behind.
#[tokio::test]
async fn a_transfer_that_dies_lands_in_error_and_is_then_removed() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_transfer_that_dies_lands_in_error_and_is_then_removed");
    };
    let info = brother();
    let daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.ready(&info).await;

    let feed = open_next_job(&daemon.handle(), JobRegistry::FIRST_ID);
    daemon.press(&info.id, 1);

    feed.page(page(0)).unwrap();
    let path = await_job(&client, &info.id, JobRegistry::FIRST_ID).await;
    let job = job_proxy(&client, &info.id, JobRegistry::FIRST_ID).await;
    let mut announced = changes(&client, path.clone()).await;

    // The device stops answering half way through the batch. A stream that merely ended
    // here would read as a successful one-page scan, which is why the items are results.
    feed.fail(BackendError::NotReachable {
        scanner: info.id.clone(),
        detail: "the device stopped answering after page 1".to_owned(),
    })
    .unwrap();
    feed.end();

    assert_eq!(next_string(&mut announced, "State").await, "error");
    assert_eq!(job.state().await.unwrap(), "error");
    let message = job.error().await.unwrap();
    assert!(
        message.contains("stopped answering after page 1"),
        "the backend's own message has to reach the client: {message}"
    );
    // The capture never completed, so there is no successful profile output.
    assert!(job.result().await.unwrap().is_empty());
    assert_eq!(job.page_count().await.unwrap(), 1);

    // And then it goes, with an `InterfacesRemoved` a client can act on.
    await_removed(&client, &path).await;
    assert!(job_paths(&client).await.is_empty());
    assert!(
        job.state().await.is_err(),
        "a call against a removed job is UnknownObject, not a stale reply"
    );

    daemon.shutdown().await;
}

/// Acceptance: an image-profile job that fails after writing two pages keeps both files
/// and reports them in `Result` alongside `State="error"`.
#[tokio::test]
async fn image_profile_failure_keeps_written_paths_in_result() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("image_profile_failure_keeps_written_paths_in_result");
    };
    let info = brother();
    let daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.ready(&info).await;

    let out = std::env::temp_dir().join(format!(
        "scanbus-image-error-result-{}-{}",
        std::process::id(),
        JobRegistry::FIRST_ID
    ));
    fs::create_dir_all(&out).unwrap();

    let image = profile_proxy(&client, scanbus_core::ProfileKind::Image).await;
    image
        .set_options(HashMap::from([
            (
                "format".to_owned(),
                OwnedValue::try_from(ZValue::from("jpeg")).unwrap(),
            ),
            (
                "output_folder".to_owned(),
                OwnedValue::try_from(ZValue::from(out.to_string_lossy().to_string())).unwrap(),
            ),
        ]))
        .await
        .unwrap();

    let button = button_proxy(&client, &info.id, 1).await;
    button.set_profile("image").await.unwrap();

    let feed = open_next_job(&daemon.handle(), JobRegistry::FIRST_ID);
    daemon.press(&info.id, 1);
    feed.page(jpeg_page(0, 20)).unwrap();
    let path = await_job(&client, &info.id, JobRegistry::FIRST_ID).await;
    let job = job_proxy(&client, &info.id, JobRegistry::FIRST_ID).await;
    let mut announced = changes(&client, path.clone()).await;

    feed.page(jpeg_page(1, 80)).unwrap();
    feed.fail(BackendError::NotReachable {
        scanner: info.id.clone(),
        detail: "dropped after two pages".to_owned(),
    })
    .unwrap();
    feed.end();

    assert_eq!(next_string(&mut announced, "State").await, "error");
    let result = job.result().await.unwrap();
    let paths = result
        .get("paths")
        .and_then(|value| Vec::<String>::try_from(value.clone()).ok())
        .unwrap();
    assert_eq!(paths.len(), 2);

    for file in &paths {
        assert!(
            std::path::Path::new(file).exists(),
            "written page is missing: {file}"
        );
        let bytes = fs::read(file).unwrap();
        assert_eq!(
            image::guess_format(&bytes).unwrap(),
            image::ImageFormat::Jpeg
        );
    }

    await_removed(&client, &path).await;
    let _ = fs::remove_dir_all(&out);
    daemon.shutdown().await;
}

/// Acceptance: if the transfer fails mid-run while `document` is assigned, the job still
/// reports a partial PDF path in `Result` and carries the backend error in `Error`.
#[tokio::test]
async fn document_profile_failure_keeps_partial_pdf_in_result() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("document_profile_failure_keeps_partial_pdf_in_result");
    };
    let info = brother();
    let daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.ready(&info).await;

    let out = std::env::temp_dir().join(format!(
        "scanbus-document-error-result-{}-{}",
        std::process::id(),
        JobRegistry::FIRST_ID
    ));
    fs::create_dir_all(&out).unwrap();

    let document = profile_proxy(&client, scanbus_core::ProfileKind::Document).await;
    document
        .set_options(HashMap::from([(
            "output_folder".to_owned(),
            OwnedValue::try_from(ZValue::from(out.to_string_lossy().to_string())).unwrap(),
        )]))
        .await
        .unwrap();

    let button = button_proxy(&client, &info.id, 2).await;
    button.set_profile("document").await.unwrap();

    let feed = open_next_job(&daemon.handle(), JobRegistry::FIRST_ID);
    daemon.press(&info.id, 2);
    feed.page(jpeg_page(0, 30)).unwrap();
    feed.page(jpeg_page(1, 90)).unwrap();
    let path = await_job(&client, &info.id, JobRegistry::FIRST_ID).await;
    let job = job_proxy(&client, &info.id, JobRegistry::FIRST_ID).await;
    let mut announced = changes(&client, path.clone()).await;

    feed.fail(BackendError::NotReachable {
        scanner: info.id.clone(),
        detail: "dropped while feeding ADF".to_owned(),
    })
    .unwrap();
    feed.end();

    assert_eq!(next_string(&mut announced, "State").await, "error");
    let message = job.error().await.unwrap();
    assert!(message.contains("dropped while feeding ADF"), "{message}");

    let result = job.result().await.unwrap();
    let pdf = String::try_from(result["path"].clone()).unwrap();
    assert!(pdf.ends_with(".pdf"));
    assert!(std::path::Path::new(&pdf).exists(), "missing output: {pdf}");

    await_removed(&client, &path).await;
    let _ = fs::remove_dir_all(out);
    daemon.shutdown().await;
}

/// Checklist: a finished job survives the retention window and then goes, so a client
/// that reacts to `State="done"` can still read `Result`.
#[tokio::test]
async fn a_finished_job_is_readable_before_it_is_removed() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_finished_job_is_readable_before_it_is_removed");
    };
    let info = brother();
    let daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.ready(&info).await;

    let feed = open_next_job(&daemon.handle(), JobRegistry::FIRST_ID);
    daemon.press(&info.id, 0);
    feed.page(page(0)).unwrap();
    let path = await_job(&client, &info.id, JobRegistry::FIRST_ID).await;

    let job = job_proxy(&client, &info.id, JobRegistry::FIRST_ID).await;
    let mut announced = changes(&client, path.clone()).await;
    feed.end();

    assert_eq!(next_string(&mut announced, "State").await, "processing");
    assert_eq!(next_string(&mut announced, "State").await, "done");

    // Still there, and still answering: the window is what makes reading `Result` after
    // the signal a supported thing to do rather than a race.
    assert_eq!(job.state().await.unwrap(), "done");
    assert_eq!(job_paths(&client).await, vec![path.clone()]);

    await_removed(&client, &path).await;
    assert!(job_paths(&client).await.is_empty());

    daemon.shutdown().await;
}

/// Checklist: job ids are unique per scanner and never reused within a daemon lifetime,
/// and jobs on different scanners are independent.
#[tokio::test]
async fn ids_are_never_reused_and_two_scanners_do_not_collide() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("ids_are_never_reused_and_two_scanners_do_not_collide");
    };
    let one = brother();
    let two = ScannerInfo {
        id: ScannerId::from_backend("mock", "usb:001:003").unwrap(),
        address: "usb:001:003".to_owned(),
        ..brother()
    };
    let daemon = Daemon::start(&bus, [one.clone(), two.clone()]).await;
    let client = bus.connect().await;
    daemon.ready(&one).await;
    daemon.ready(&two).await;

    // Two scans in flight at once, on two devices. Each listener task is its own, so
    // neither waits for the other.
    let first = open_next_job(&daemon.handle(), JobRegistry::FIRST_ID);
    daemon.press(&one.id, 0);
    first.page(page(0)).unwrap();
    let first_path = await_job(&client, &one.id, JobRegistry::FIRST_ID).await;

    let second = open_next_job(&daemon.handle(), JobRegistry::FIRST_ID + 1);
    daemon.press(&two.id, 3);
    second.page(page(0)).unwrap();
    let second_path = await_job(&client, &two.id, JobRegistry::FIRST_ID + 1).await;

    let mut both = job_paths(&client).await;
    both.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    let mut expected = vec![first_path, second_path];
    expected.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    assert_eq!(both, expected, "one id is never handed out twice");

    // The id counter does not go backwards when a job finishes, either.
    first.end();
    second.end();
    let third = open_next_job(&daemon.handle(), JobRegistry::FIRST_ID + 2);
    daemon.press(&one.id, 0);
    third.page(page(0)).unwrap();
    await_job(&client, &one.id, JobRegistry::FIRST_ID + 2).await;

    third.end();
    daemon.shutdown().await;
}

/// Checklist: the precedence order of §3 decides `Job1.Profile`, resolved once, at
/// creation — the button, then the session profile, then `DefaultProfile`.
#[tokio::test]
async fn the_profile_is_resolved_by_precedence_at_the_trigger() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("the_profile_is_resolved_by_precedence_at_the_trigger");
    };
    let info = brother();
    let daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.ready(&info).await;

    let scanner = scanner_proxy(&client, &info.id).await;
    scanner.set_default_profile("image").await.unwrap();

    // Nothing on the key and no session profile: the scanner-wide fallback wins.
    let mut job_id = JobRegistry::FIRST_ID;
    let feed = open_next_job(&daemon.handle(), job_id);
    daemon.press(&info.id, 1);
    feed.page(page(0)).unwrap();
    await_job(&client, &info.id, job_id).await;
    assert_eq!(
        job_proxy(&client, &info.id, job_id)
            .await
            .profile()
            .await
            .unwrap(),
        "image"
    );
    feed.end();

    // A `Connect()` with a session profile outranks the default.
    scanner
        .connect(HashMap::from([(
            "profile".to_owned(),
            OwnedValue::try_from(ZValue::from("document")).unwrap(),
        )]))
        .await
        .unwrap();

    job_id += 1;
    let feed = open_next_job(&daemon.handle(), job_id);
    daemon.press(&info.id, 1);
    feed.page(page(0)).unwrap();
    await_job(&client, &info.id, job_id).await;
    assert_eq!(
        job_proxy(&client, &info.id, job_id)
            .await
            .profile()
            .await
            .unwrap(),
        "document"
    );
    feed.end();

    // And the key's own assignment outranks both.
    button_proxy(&client, &info.id, 1)
        .await
        .set_profile("image")
        .await
        .unwrap();
    scanner.set_default_profile("document").await.unwrap();

    job_id += 1;
    let feed = open_next_job(&daemon.handle(), job_id);
    daemon.press(&info.id, 1);
    feed.page(page(0)).unwrap();
    await_job(&client, &info.id, job_id).await;
    assert_eq!(
        job_proxy(&client, &info.id, job_id)
            .await
            .profile()
            .await
            .unwrap(),
        "image",
        "Button1.Profile wins over the session profile and over DefaultProfile"
    );
    feed.end();

    daemon.shutdown().await;
}

/// Checklist: a press whose transfer yields nothing publishes no object at all — §1
/// creates a `Job1` "when data is received", and no data is no job.
#[tokio::test]
async fn a_trigger_that_delivers_no_data_publishes_nothing() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_trigger_that_delivers_no_data_publishes_nothing");
    };
    let info = brother();
    let daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.ready(&info).await;

    // Nothing is queued for the job the daemon is about to mint, so `fetch_pages` comes
    // back with `UnknownJob` — the vendor tool has nothing to hand over.
    daemon.press(&info.id, 0);

    // A second press, this one with data, so the assertion is about the first having
    // produced nothing rather than about the daemon not having got there yet.
    let feed = open_next_job(&daemon.handle(), JobRegistry::FIRST_ID + 1);
    daemon.press(&info.id, 0);
    feed.page(page(0)).unwrap();
    let path = await_job(&client, &info.id, JobRegistry::FIRST_ID + 1).await;

    assert_eq!(
        job_paths(&client).await,
        vec![path],
        "the press that produced no data left no object behind"
    );

    feed.end();
    daemon.shutdown().await;
}

/// Checklist: a scan in flight is not killed by the thing that stopped listening.
///
/// `Disconnect()` aborts the listener task, and that task is what awaits the sink. If the
/// job died with it, the object would sit in `"receiving"` for ever with nothing left to
/// move it — the registered object left behind that 2.6 exists to rule out. It runs in a
/// task of its own precisely so an abort *detaches* it.
#[tokio::test]
async fn disconnecting_mid_scan_does_not_strand_the_job() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("disconnecting_mid_scan_does_not_strand_the_job");
    };
    let info = brother();
    let daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.ready(&info).await;

    let feed = open_next_job(&daemon.handle(), JobRegistry::FIRST_ID);
    daemon.press(&info.id, 0);
    feed.page(page(0)).unwrap();
    let path = await_job(&client, &info.id, JobRegistry::FIRST_ID).await;
    let mut announced = changes(&client, path.clone()).await;

    // The user disconnects while the batch is still feeding.
    scanner_proxy(&client, &info.id)
        .await
        .disconnect()
        .await
        .unwrap();

    // The scan carries on: the remaining sheet arrives and the job finishes normally.
    feed.page(page(1)).unwrap();
    assert_eq!(next_u32(&mut announced, "PageCount").await, 2);
    feed.end();

    assert_eq!(next_string(&mut announced, "State").await, "processing");
    assert_eq!(next_string(&mut announced, "State").await, "done");

    // And it is still cleaned up afterwards, which is the half an abort would have lost.
    await_removed(&client, &path).await;
    assert!(job_paths(&client).await.is_empty());

    daemon.shutdown().await;
}

/// Checklist: the `Button` property of a scan the host started is `-1`, not a key index.
///
/// Driven through [`JobRegistry`] directly rather than over the bus, because §3's
/// `Scan()` is optional and has no method yet — the backend seam has nothing that *starts*
/// a scan, only `fetch_pages` for one the device already began. What 2.6 owns is that a
/// job with no key behind it is representable and says so, and that is what is asserted.
#[tokio::test]
async fn a_host_driven_job_reports_button_minus_one() {
    let Some(bus) = PrivateBus::start().await else {
        return skipped("a_host_driven_job_reports_button_minus_one");
    };
    let info = brother();
    let daemon = Daemon::start(&bus, [info.clone()]).await;
    let client = bus.connect().await;
    daemon.ready(&info).await;

    let jobs = JobRegistry::with_retention(
        Arc::clone(&daemon.objects),
        Arc::new(ProfileRegistry::ephemeral()),
        TEST_RETENTION,
    );
    let feed = daemon.handle().open_pages("host-1");

    let started = tokio::spawn({
        let backend = Arc::clone(&daemon.backend) as Arc<dyn ScannerBackend>;
        let id = info.id.clone();
        async move {
            jobs.start(
                backend,
                id,
                "host-1".to_owned(),
                JobTrigger::Host,
                Some(scanbus_core::ProfileKind::Document),
                Default::default(),
            )
            .await
        }
    });

    feed.page(page(0)).unwrap();
    await_job(&client, &info.id, JobRegistry::FIRST_ID).await;

    let job = job_proxy(&client, &info.id, JobRegistry::FIRST_ID).await;
    assert_eq!(job.button().await.unwrap(), -1);
    assert_eq!(job.profile().await.unwrap(), "document");

    feed.end();
    assert_eq!(started.await.unwrap(), Some(JobRegistry::FIRST_ID));

    daemon.shutdown().await;
}
