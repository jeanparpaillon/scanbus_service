//! [`MockBackend`]: a [`ScannerBackend`] every answer of which a test decides.
//!
//! Not a stub. This is the test harness for workstreams 2 and 3: the D-Bus surface has
//! to be drivable end to end — discovery, a failed install, a button press on key 2,
//! three pages out of an ADF — from a test with no bus, no scanner and no sleeping. A
//! stub that always returns `Ok(vec![])` would leave every one of those acceptance
//! criteria reading "plug in a printer".
//!
//! The control side is [`MockHandle`], a cheap clone of the same shared state, obtained
//! from [`MockBackend::handle`]. Its methods are **synchronous on purpose**: "emit a
//! button press on key 2 now" is a statement a test makes between two `await`s, not
//! another future to schedule.
//!
//! ```text
//! let backend = MockBackend::with_scanners([sample_scanner()]);
//! let handle = backend.handle();
//! let mut events = backend.start_listening(&scanner).await?;
//! handle.press_button(&scanner.id, 2)?;      // synchronous
//! assert_eq!(events.next().await.unwrap().button_index, 2);
//! ```
//!
//! The tests at the bottom of this file are the worked examples, and are also the
//! acceptance criteria of issue 1.3.

use std::collections::{BTreeMap, BTreeSet};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures_core::Stream;
use futures_core::stream::BoxStream;
use thiserror::Error;
use tokio::sync::mpsc;

use super::{ButtonPressedEvent, PairingProgress, ScannerBackend};
use crate::error::BackendError;
use crate::model::{
    ButtonsCapability, Capabilities, ColorMode, ProfileKind, RawPage, ScannerId, ScannerInfo,
    Source, Status, Value,
};

/// How many presses the mock buffers before [`MockHandle::press_button`] refuses.
///
/// Bounded rather than unbounded so that a consumer which stops reading is a visible
/// [`MockError::ListenerFull`] in a test instead of unbounded growth in the daemon.
const EVENT_BUFFER: usize = 16;

/// How many pages a [`PageFeed`] buffers before [`PageFeed::page`] refuses.
///
/// Same reasoning as [`EVENT_BUFFER`], and the same size: a job that has stopped reading
/// its pages should be a failed assertion, not a growing queue.
const PAGE_BUFFER: usize = 16;

/// A scanner shaped like the Brother MFC of [`scanbus-dbus-api.md`] §5: four fixed
/// keys, no host-settable labels, flatbed plus duplex ADF.
///
/// Shared rather than rebuilt per test because the object tree that hangs off it — four
/// `Button1` objects — is what most acceptance tests assert on.
///
/// [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md
pub fn sample_scanner() -> ScannerInfo {
    ScannerInfo {
        id: ScannerId::from_backend("mock", "usb:001:002").expect("a legal mock id"),
        name: "Brother MFC-L2710DW".to_owned(),
        backend: "proprietary:brother".to_owned(),
        address: "usb:001:002".to_owned(),
        capabilities: Capabilities {
            resolutions: vec![100, 200, 300, 600],
            color_modes: vec![ColorMode::Color, ColorMode::Gray, ColorMode::Bw],
            sources: vec![Source::Flatbed, Source::Adf],
            duplex: true,
            buttons: ButtonsCapability {
                count: 4,
                label_configurable: false,
            },
            ..Capabilities::default()
        },
        status: Status::Online,
    }
}

/// A backend under a test's control.
///
/// Clone to hand the same backend to several tasks; every clone shares the state
/// [`MockBackend::handle`] drives.
#[derive(Clone)]
pub struct MockBackend {
    id: &'static str,
    state: Arc<Mutex<MockState>>,
}

/// The control side of a [`MockBackend`].
#[derive(Clone)]
pub struct MockHandle {
    state: Arc<Mutex<MockState>>,
}

/// What a [`MockBackend`] was asked to do, in order.
///
/// The log is how a test asserts on calls that have no return value worth checking —
/// that `stop_listening` ran and found nothing to stop, that a key was cleared rather
/// than reassigned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockCall {
    /// [`ScannerBackend::discover`].
    Discover,
    /// [`ScannerBackend::ensure_installed`], recorded when the call starts — so a
    /// cancelled install still appears.
    EnsureInstalled(ScannerId),
    /// [`ScannerBackend::start_listening`].
    StartListening(ScannerId),
    /// [`ScannerBackend::stop_listening`], with whether there was a live listener.
    StopListening {
        /// The scanner.
        scanner: ScannerId,
        /// `false` when nothing was listening — the no-op case the trait requires.
        was_listening: bool,
    },
    /// [`ScannerBackend::forget`].
    Forget(ScannerId),
    /// [`ScannerBackend::set_button_mapping`]; `profile: None` is a cleared key.
    SetButtonMapping {
        /// The scanner.
        scanner: ScannerId,
        /// Position in the physical menu.
        button_index: u32,
        /// The profile assigned, or `None` when the key was cleared.
        profile: Option<ProfileKind>,
    },
    /// [`ScannerBackend::fetch_pages`], recorded whether or not the job existed.
    FetchPages {
        /// The scanner.
        scanner: ScannerId,
        /// The job id asked for.
        job: String,
    },
}

/// What a key is currently mapped to in the mock's "config file".
#[derive(Debug, Clone, PartialEq)]
pub struct ButtonMapping {
    /// The assigned profile. A cleared key has no [`ButtonMapping`] at all.
    pub profile: ProfileKind,
    /// The options written alongside it.
    pub options: BTreeMap<String, Value>,
}

/// A [`MockHandle`] operation that does not apply in the current state.
///
/// Distinct from [`BackendError`]: these are complaints about the *test*, not failures
/// a backend could report. Pressing a key nobody is listening for is a test that has
/// its ordering wrong, and should say so rather than silently do nothing.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MockError {
    /// No listener has been started for this scanner.
    #[error("no listener is running for {0}")]
    NotListening(ScannerId),
    /// A listener was started, but the consumer dropped the stream.
    #[error("the listener stream for {0} has been dropped")]
    ListenerDropped(ScannerId),
    /// The consumer is not reading: [`EVENT_BUFFER`] presses are already queued.
    #[error("the listener queue for {0} is full")]
    ListenerFull(ScannerId),
    /// Nobody is reading the pages: the job was never fetched, or it has finished.
    #[error("nothing is reading the pages of job {0:?}")]
    PagesDropped(String),
    /// The consumer is not reading: [`PAGE_BUFFER`] pages are already queued.
    #[error("the page queue for job {0:?} is full")]
    PagesFull(String),
}

/// Where [`ScannerBackend::fetch_pages`] gets one job's pages from.
///
/// Two shapes, because tests want two different things. A batch is the whole scan decided
/// up front, which is what an assertion about the *outcome* needs; a channel is a scan a
/// test delivers one page at a time, which is the only way to observe a `Job1` while it
/// is still `"receiving"` — the mock's streams are otherwise ready on every poll, so a
/// three-page batch is over before a client can read `PageCount`.
enum PageSource {
    /// Every item, decided before the fetch.
    Batch(Vec<Result<RawPage, BackendError>>),
    /// Items as a [`PageFeed`] sends them; the stream ends when the feed is dropped.
    Feed(mpsc::Receiver<Result<RawPage, BackendError>>),
}

/// The writing end of a page stream a test drives page by page.
///
/// Obtained from [`MockHandle::open_pages`], and **synchronous on purpose**, like
/// [`MockHandle::press_button`]: "page 2 arrives now" is a statement a test makes between
/// two `await`s. Dropping it ends the stream, which is what an ADF running out of sheets
/// looks like to the consumer.
pub struct PageFeed {
    job_id: String,
    sender: mpsc::Sender<Result<RawPage, BackendError>>,
}

impl PageFeed {
    /// Delivers one page.
    ///
    /// # Errors
    ///
    /// [`MockError::PagesDropped`] if nothing is reading the stream any more,
    /// [`MockError::PagesFull`] if the consumer stopped reading.
    pub fn page(&self, page: RawPage) -> Result<(), MockError> {
        self.send(Ok(page))
    }

    /// Fails the transfer, as a device that stops answering mid-scan does.
    ///
    /// The stream yields this and then ends when the feed is dropped; nothing is expected
    /// after an `Err`, which is the contract
    /// [`fetch_pages`](ScannerBackend::fetch_pages) states.
    ///
    /// # Errors
    ///
    /// As [`PageFeed::page`].
    pub fn fail(&self, error: BackendError) -> Result<(), MockError> {
        self.send(Err(error))
    }

    /// Ends the stream, as the last sheet of an ADF batch does.
    ///
    /// The same thing dropping the feed does, spelled out where a test wants the end of
    /// capture to be a step rather than a scope ending.
    pub fn end(self) {}

    fn send(&self, item: Result<RawPage, BackendError>) -> Result<(), MockError> {
        self.sender.try_send(item).map_err(|error| match error {
            mpsc::error::TrySendError::Closed(_) => MockError::PagesDropped(self.job_id.clone()),
            mpsc::error::TrySendError::Full(_) => MockError::PagesFull(self.job_id.clone()),
        })
    }
}

/// Everything both halves share.
struct MockState {
    discovery: Result<Vec<ScannerInfo>, BackendError>,
    install_packages: Vec<String>,
    install_failure: Option<BackendError>,
    installed: BTreeSet<ScannerId>,
    install_attempts: u32,
    listeners: BTreeMap<ScannerId, mpsc::Sender<ButtonPressedEvent>>,
    pages: BTreeMap<String, PageSource>,
    mappings: BTreeMap<(ScannerId, u32), ButtonMapping>,
    mapping_failure: Option<BackendError>,
    calls: Vec<MockCall>,
}

impl MockState {
    fn new(discovery: Vec<ScannerInfo>) -> Self {
        Self {
            discovery: Ok(discovery),
            install_packages: Vec::new(),
            install_failure: None,
            installed: BTreeSet::new(),
            install_attempts: 0,
            listeners: BTreeMap::new(),
            pages: BTreeMap::new(),
            mappings: BTreeMap::new(),
            mapping_failure: None,
            calls: Vec::new(),
        }
    }
}

impl MockBackend {
    /// The identifier [`ScannerBackend::id`] reports.
    pub const ID: &'static str = "mock";

    /// A backend that discovers nothing until a test says otherwise.
    pub fn new() -> Self {
        Self::with_scanners([])
    }

    /// A backend whose [`discover`](ScannerBackend::discover) yields `scanners`.
    pub fn with_scanners(scanners: impl IntoIterator<Item = ScannerInfo>) -> Self {
        Self {
            id: Self::ID,
            state: Arc::new(Mutex::new(MockState::new(scanners.into_iter().collect()))),
        }
    }

    /// The same backend, reporting `id` from [`ScannerBackend::id`].
    ///
    /// Backend ids have to be unique within the daemon's backend list — it is how a
    /// `StartDiscovery` filter names one — so anything testing *several* backends at
    /// once needs more than one name. The deduplication case of
    /// [`scanbus-dbus-api.md`] §9 is exactly that: the same device found twice, by two
    /// backends, and the test has to be able to say which one won.
    ///
    /// [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md
    pub fn with_id(mut self, id: &'static str) -> Self {
        self.id = id;
        self
    }

    /// The control side. Cheap, and any number of them may exist.
    pub fn handle(&self) -> MockHandle {
        MockHandle {
            state: Arc::clone(&self.state),
        }
    }

    fn state(&self) -> MutexGuard<'_, MockState> {
        self.state.lock().expect("mock state lock poisoned")
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MockHandle {
    /// Makes [`discover`](ScannerBackend::discover) yield `scanners`, replacing whatever
    /// it yielded before.
    pub fn set_discovery(&self, scanners: impl IntoIterator<Item = ScannerInfo>) {
        self.state().discovery = Ok(scanners.into_iter().collect());
    }

    /// Makes [`discover`](ScannerBackend::discover) fail, every time, with `error`.
    pub fn fail_discovery(&self, error: BackendError) {
        self.state().discovery = Err(error);
    }

    /// Sets the packages [`ensure_installed`](ScannerBackend::ensure_installed) reports
    /// installing, in order.
    ///
    /// Empty by default: a system where everything is already present, which is the
    /// case most tests want and the one where `PairingState` never reaches
    /// `"installing_backend"`.
    pub fn set_install_packages(&self, packages: impl IntoIterator<Item = impl Into<String>>) {
        self.state().install_packages = packages.into_iter().map(Into::into).collect();
    }

    /// Makes [`ensure_installed`](ScannerBackend::ensure_installed) fail with `error`,
    /// after emitting its progress and a final [`PairingProgress::Failed`].
    pub fn fail_ensure_installed(&self, error: BackendError) {
        self.state().install_failure = Some(error);
    }

    /// Undoes [`MockHandle::fail_ensure_installed`], so the next call succeeds.
    pub fn succeed_ensure_installed(&self) {
        self.state().install_failure = None;
    }

    /// Whether the mock has recorded a completed install for this scanner.
    ///
    /// Set only once [`ensure_installed`](ScannerBackend::ensure_installed) has got past
    /// every step, which is what makes it the assertion for the cancellation contract:
    /// a dropped call leaves this `false`.
    pub fn is_installed(&self, scanner_id: &ScannerId) -> bool {
        self.state().installed.contains(scanner_id)
    }

    /// How many times [`ensure_installed`](ScannerBackend::ensure_installed) has been
    /// entered, cancelled runs included.
    pub fn install_attempts(&self) -> u32 {
        self.state().install_attempts
    }

    /// Emits a press of `button_index` on `scanner_id`, now.
    ///
    /// # Errors
    ///
    /// [`MockError::NotListening`] if no listener was started,
    /// [`MockError::ListenerDropped`] if the consumer dropped the stream,
    /// [`MockError::ListenerFull`] if it stopped reading.
    pub fn press_button(&self, scanner_id: &ScannerId, button_index: u32) -> Result<(), MockError> {
        let state = self.state();
        let sender = state
            .listeners
            .get(scanner_id)
            .ok_or_else(|| MockError::NotListening(scanner_id.clone()))?;

        sender
            .try_send(ButtonPressedEvent::now(scanner_id.clone(), button_index))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Closed(_) => {
                    MockError::ListenerDropped(scanner_id.clone())
                }
                mpsc::error::TrySendError::Full(_) => MockError::ListenerFull(scanner_id.clone()),
            })
    }

    /// Whether a listener is running *and* its stream is still held.
    pub fn is_listening(&self, scanner_id: &ScannerId) -> bool {
        self.state()
            .listeners
            .get(scanner_id)
            .is_some_and(|sender| !sender.is_closed())
    }

    /// Ends the listener stream from the backend's side, as a device going away would.
    ///
    /// The consumer sees the stream finish. Returns whether there was one to end.
    pub fn end_listener(&self, scanner_id: &ScannerId) -> bool {
        self.state().listeners.remove(scanner_id).is_some()
    }

    /// Queues the pages [`fetch_pages`](ScannerBackend::fetch_pages) will hand over for
    /// `job_id`, replacing any already queued for it.
    ///
    /// The whole scan at once: the stream yields them and ends. Use
    /// [`MockHandle::open_pages`] where the test has to observe the job *between* pages.
    pub fn feed_pages(&self, job_id: impl Into<String>, pages: impl IntoIterator<Item = RawPage>) {
        self.feed_page_results(job_id, pages.into_iter().map(Ok));
    }

    /// The same, for a transfer that ends in a failure rather than in a last page.
    ///
    /// `pages` arrive, then `error`, then the stream ends — the "device dropped off
    /// mid-transfer" case, without a device to unplug.
    pub fn feed_pages_then_fail(
        &self,
        job_id: impl Into<String>,
        pages: impl IntoIterator<Item = RawPage>,
        error: BackendError,
    ) {
        self.feed_page_results(job_id, pages.into_iter().map(Ok).chain([Err(error)]));
    }

    /// Queues an arbitrary sequence of page results for `job_id`.
    pub fn feed_page_results(
        &self,
        job_id: impl Into<String>,
        items: impl IntoIterator<Item = Result<RawPage, BackendError>>,
    ) {
        self.state().pages.insert(
            job_id.into(),
            PageSource::Batch(items.into_iter().collect()),
        );
    }

    /// Opens a page stream for `job_id` that the test feeds page by page.
    ///
    /// The returned [`PageFeed`] is what makes a scan observable while it is running: the
    /// job stays in `"receiving"` for exactly as long as the feed is alive, so a test can
    /// read `PageCount`, write a `Button1.Profile` or call `GetManagedObjects` in the
    /// middle of one. Dropping the feed — or calling [`PageFeed::end`] — is the last
    /// sheet of the batch.
    ///
    /// Replaces anything already queued for that id.
    pub fn open_pages(&self, job_id: impl Into<String>) -> PageFeed {
        let job_id = job_id.into();
        let (sender, receiver) = mpsc::channel(PAGE_BUFFER);

        self.state()
            .pages
            .insert(job_id.clone(), PageSource::Feed(receiver));

        PageFeed { job_id, sender }
    }

    /// Makes [`set_button_mapping`](ScannerBackend::set_button_mapping) fail with
    /// `error`, every time, leaving the recorded mapping untouched.
    ///
    /// The condition 2.5 is written against: writing `Button1.Profile` rewrites a vendor
    /// config file, and on Brother that file lives under `/opt` and may not be writable
    /// (5.4). A test needs the failing write to be reachable without a read-only `/opt`.
    pub fn fail_button_mapping(&self, error: BackendError) {
        self.state().mapping_failure = Some(error);
    }

    /// Undoes [`MockHandle::fail_button_mapping`], so the next write lands.
    pub fn succeed_button_mapping(&self) {
        self.state().mapping_failure = None;
    }

    /// What key `button_index` is mapped to, or `None` when it is unassigned.
    pub fn button_mapping(
        &self,
        scanner_id: &ScannerId,
        button_index: u32,
    ) -> Option<ButtonMapping> {
        self.state()
            .mappings
            .get(&(scanner_id.clone(), button_index))
            .cloned()
    }

    /// Every call the backend has taken, oldest first.
    pub fn calls(&self) -> Vec<MockCall> {
        self.state().calls.clone()
    }

    /// Forgets the call log, so a test can assert on one phase at a time.
    pub fn clear_calls(&self) {
        self.state().calls.clear();
    }

    fn state(&self) -> MutexGuard<'_, MockState> {
        self.state.lock().expect("mock state lock poisoned")
    }
}

#[async_trait]
impl ScannerBackend for MockBackend {
    fn id(&self) -> &'static str {
        self.id
    }

    async fn discover(&self) -> Result<Vec<ScannerInfo>, BackendError> {
        let mut state = self.state();
        state.calls.push(MockCall::Discover);
        state.discovery.clone()
    }

    async fn ensure_installed(
        &self,
        scanner: &ScannerInfo,
        progress: mpsc::Sender<PairingProgress>,
    ) -> Result<(), BackendError> {
        // The lock is taken and released before the first await: holding a std Mutex
        // across one is how a backend that looks fine in a test deadlocks the daemon.
        let (packages, failure) = {
            let mut state = self.state();
            state
                .calls
                .push(MockCall::EnsureInstalled(scanner.id.clone()));
            state.install_attempts += 1;

            let packages = if state.installed.contains(&scanner.id) {
                // Already installed: the idempotent path, Checking straight to Ready.
                Vec::new()
            } else {
                state.install_packages.clone()
            };
            (packages, state.install_failure.clone())
        };

        // A closed channel is not a failure: the daemon may have stopped watching
        // (CancelPairing), and the install still has to finish or unwind on its own.
        let _ = progress
            .send(PairingProgress::Checking {
                message: format!("checking dependencies for {}", scanner.name),
            })
            .await;

        for package in packages {
            let _ = progress
                .send(PairingProgress::Installing {
                    package,
                    percent: None,
                })
                .await;
        }

        if let Some(error) = failure {
            // Before the Err, so a client watching PropertiesChanged and a caller
            // reading the Result never disagree.
            let _ = progress
                .send(PairingProgress::Failed {
                    message: error.to_string(),
                })
                .await;
            return Err(error);
        }

        // Committed only here, once every step has run: a future dropped before this
        // point leaves the scanner uninstalled and the next call redoes the work.
        self.state().installed.insert(scanner.id.clone());
        let _ = progress.send(PairingProgress::Ready).await;
        Ok(())
    }

    async fn start_listening(
        &self,
        scanner: &ScannerInfo,
    ) -> Result<BoxStream<'static, ButtonPressedEvent>, BackendError> {
        let mut state = self.state();
        state
            .calls
            .push(MockCall::StartListening(scanner.id.clone()));

        // A live listener is a conflict; one whose stream the caller has dropped is
        // just stale, and is replaced — the trait allows stopping by drop alone.
        if state
            .listeners
            .get(&scanner.id)
            .is_some_and(|sender| !sender.is_closed())
        {
            return Err(BackendError::Busy(scanner.id.clone()));
        }

        let (sender, receiver) = mpsc::channel(EVENT_BUFFER);
        state.listeners.insert(scanner.id.clone(), sender);
        Ok(Box::pin(EventStream(receiver)))
    }

    async fn stop_listening(&self, scanner_id: &ScannerId) -> Result<(), BackendError> {
        let mut state = self.state();
        let was_listening = state
            .listeners
            .remove(scanner_id)
            .is_some_and(|sender| !sender.is_closed());

        state.calls.push(MockCall::StopListening {
            scanner: scanner_id.clone(),
            was_listening,
        });
        Ok(())
    }

    async fn forget(&self, scanner_id: &ScannerId) -> Result<(), BackendError> {
        let mut state = self.state();
        state.calls.push(MockCall::Forget(scanner_id.clone()));
        Ok(())
    }

    async fn set_button_mapping(
        &self,
        scanner_id: &ScannerId,
        button_index: u32,
        profile: Option<ProfileKind>,
        options: &BTreeMap<String, Value>,
    ) -> Result<(), BackendError> {
        let mut state = self.state();
        state.calls.push(MockCall::SetButtonMapping {
            scanner: scanner_id.clone(),
            button_index,
            profile,
        });

        // Recorded first, then refused: a write that reached the backend and failed there
        // is exactly what 2.5 has to tell apart from one the daemon rejected on its own.
        if let Some(error) = state.mapping_failure.clone() {
            return Err(error);
        }

        let key = (scanner_id.clone(), button_index);
        match profile {
            Some(profile) => state.mappings.insert(
                key,
                ButtonMapping {
                    profile,
                    options: options.clone(),
                },
            ),
            // Clearing a key removes its entry, as rewriting the vendor config would.
            None => state.mappings.remove(&key),
        };
        Ok(())
    }

    async fn fetch_pages(
        &self,
        scanner_id: &ScannerId,
        job_id: &str,
    ) -> Result<BoxStream<'static, Result<RawPage, BackendError>>, BackendError> {
        let mut state = self.state();
        state.calls.push(MockCall::FetchPages {
            scanner: scanner_id.clone(),
            job: job_id.to_owned(),
        });

        // Removing rather than reading is the trait's once-per-job rule: a second call
        // for the same id is indistinguishable from one that never existed.
        let source = state
            .pages
            .remove(job_id)
            .ok_or_else(|| BackendError::UnknownJob {
                scanner: scanner_id.clone(),
                job: job_id.to_owned(),
            })?;

        Ok(match source {
            PageSource::Batch(items) => Box::pin(PageStream(items.into_iter())),
            PageSource::Feed(receiver) => Box::pin(PageFeedStream(receiver)),
        })
    }
}

/// The button stream: a channel the handle writes into.
struct EventStream(mpsc::Receiver<ButtonPressedEvent>);

impl Stream for EventStream {
    type Item = ButtonPressedEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().0.poll_recv(cx)
    }
}

/// The batch page stream: fed pages, then done. Ready every poll — a scan that a test has
/// to wait for is a test with a timer in it.
struct PageStream(std::vec::IntoIter<Result<RawPage, BackendError>>);

impl Stream for PageStream {
    type Item = Result<RawPage, BackendError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.get_mut().0.next())
    }
}

/// The driven page stream: a channel a [`PageFeed`] writes into, ending when it is
/// dropped.
struct PageFeedStream(mpsc::Receiver<Result<RawPage, BackendError>>);

impl Stream for PageFeedStream {
    type Item = Result<RawPage, BackendError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().0.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use futures_util::{StreamExt, poll};

    use super::*;
    use crate::model::{PageFormat, PairingState};

    fn page(index: u32) -> RawPage {
        RawPage {
            index,
            format: PageFormat::Pnm,
            resolution_dpi: 300,
            data: vec![0x42; 32],
        }
    }

    /// Everything the sender has queued, without waiting for anything.
    fn drain(receiver: &mut mpsc::Receiver<PairingProgress>) -> Vec<PairingProgress> {
        let mut steps = Vec::new();
        while let Ok(step) = receiver.try_recv() {
            steps.push(step);
        }
        steps
    }

    fn installed_packages(steps: &[PairingProgress]) -> Vec<&str> {
        steps
            .iter()
            .filter_map(|step| match step {
                PairingProgress::Installing { package, .. } => Some(package.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The acceptance criterion: the whole sequence, no bus, no timer.
    #[tokio::test]
    async fn drives_discovery_through_to_pages() {
        let scanner = sample_scanner();
        let backend = MockBackend::with_scanners([scanner.clone()]);
        let handle = backend.handle();
        handle.set_install_packages(["brscan5", "brscan-skey"]);

        assert_eq!(backend.discover().await.unwrap(), vec![scanner.clone()]);

        let (sender, mut receiver) = mpsc::channel(8);
        backend.ensure_installed(&scanner, sender).await.unwrap();
        let steps = drain(&mut receiver);

        assert!(matches!(
            steps.first(),
            Some(PairingProgress::Checking { .. })
        ));
        assert_eq!(installed_packages(&steps), ["brscan5", "brscan-skey"]);
        assert_eq!(steps.last(), Some(&PairingProgress::Ready));
        // The transition Pair() exists to publish (§9).
        assert_eq!(
            steps[1].pairing_state(),
            Some(PairingState::InstallingBackend)
        );
        assert!(handle.is_installed(&scanner.id));

        let mut events = backend.start_listening(&scanner).await.unwrap();
        assert!(handle.is_listening(&scanner.id));

        handle.press_button(&scanner.id, 2).unwrap();
        let event = events.next().await.unwrap();
        assert_eq!(event.scanner_id, scanner.id);
        assert_eq!(event.button_index, 2);

        // The job id is the daemon's; the backend only has to answer for it.
        handle.feed_pages("job-1", [page(0), page(1), page(2)]);
        let pages: Vec<RawPage> = backend
            .fetch_pages(&scanner.id, "job-1")
            .await
            .unwrap()
            .map(|page| page.expect("a batch of Ok pages yields no failure"))
            .collect()
            .await;
        assert_eq!(
            pages.iter().map(|page| page.index).collect::<Vec<_>>(),
            [0, 1, 2]
        );

        backend.stop_listening(&scanner.id).await.unwrap();
        assert!(!handle.is_listening(&scanner.id));
    }

    /// An install on a system that already has everything says so and changes nothing.
    #[tokio::test]
    async fn a_second_install_is_a_no_op_that_still_reports() {
        let scanner = sample_scanner();
        let backend = MockBackend::with_scanners([scanner.clone()]);
        let handle = backend.handle();
        handle.set_install_packages(["brscan5"]);

        let (sender, mut receiver) = mpsc::channel(8);
        backend.ensure_installed(&scanner, sender).await.unwrap();
        assert_eq!(installed_packages(&drain(&mut receiver)), ["brscan5"]);

        let (sender, mut receiver) = mpsc::channel(8);
        backend.ensure_installed(&scanner, sender).await.unwrap();
        let steps = drain(&mut receiver);
        assert!(installed_packages(&steps).is_empty());
        assert_eq!(steps.last(), Some(&PairingProgress::Ready));
    }

    /// Acceptance: dropping the stream is a legal way to stop, so the explicit stop
    /// that follows it is a no-op and not an error.
    #[tokio::test]
    async fn dropping_the_stream_makes_stop_listening_a_no_op() {
        let scanner = sample_scanner();
        let backend = MockBackend::with_scanners([scanner.clone()]);
        let handle = backend.handle();

        let events = backend.start_listening(&scanner).await.unwrap();
        assert!(handle.is_listening(&scanner.id));

        drop(events);
        assert!(!handle.is_listening(&scanner.id));
        assert_eq!(
            handle.press_button(&scanner.id, 0),
            Err(MockError::ListenerDropped(scanner.id.clone()))
        );

        handle.clear_calls();
        backend.stop_listening(&scanner.id).await.unwrap();
        // And again, now that there is not even a stale entry left.
        backend.stop_listening(&scanner.id).await.unwrap();

        assert_eq!(
            handle.calls(),
            vec![
                MockCall::StopListening {
                    scanner: scanner.id.clone(),
                    was_listening: false,
                },
                MockCall::StopListening {
                    scanner: scanner.id.clone(),
                    was_listening: false,
                },
            ]
        );
    }

    /// A dropped stream frees the scanner; a held one does not.
    #[tokio::test]
    async fn listening_twice_is_busy_until_the_first_stream_goes_away() {
        let scanner = sample_scanner();
        let backend = MockBackend::with_scanners([scanner.clone()]);

        let events = backend.start_listening(&scanner).await.unwrap();
        // `err()` rather than `unwrap_err()`: a stream is not `Debug`.
        assert_eq!(
            backend.start_listening(&scanner).await.err(),
            Some(BackendError::Busy(scanner.id.clone()))
        );

        drop(events);
        let mut events = backend.start_listening(&scanner).await.unwrap();
        backend.handle().press_button(&scanner.id, 1).unwrap();
        assert_eq!(events.next().await.unwrap().button_index, 1);
    }

    /// The backend's side going away ends the stream rather than hanging the consumer.
    #[tokio::test]
    async fn a_listener_that_ends_finishes_the_stream() {
        let scanner = sample_scanner();
        let backend = MockBackend::with_scanners([scanner.clone()]);
        let handle = backend.handle();

        let mut events = backend.start_listening(&scanner).await.unwrap();
        handle.press_button(&scanner.id, 3).unwrap();
        assert!(handle.end_listener(&scanner.id));

        // The press queued before the device went away still arrives.
        assert_eq!(events.next().await.unwrap().button_index, 3);
        assert!(events.next().await.is_none());
        assert_eq!(
            handle.press_button(&scanner.id, 3),
            Err(MockError::NotListening(scanner.id.clone()))
        );
    }

    /// Acceptance: the failure reaches the progress channel, not only the `Result`.
    #[tokio::test]
    async fn a_failed_install_reports_the_failure_as_progress_too() {
        let scanner = sample_scanner();
        let backend = MockBackend::with_scanners([scanner.clone()]);
        let handle = backend.handle();
        handle.set_install_packages(["brscan5"]);
        handle.fail_ensure_installed(BackendError::InstallFailed {
            package: "brscan5".to_owned(),
            detail: "404 from the Brother download site".to_owned(),
        });

        let (sender, mut receiver) = mpsc::channel(8);
        let error = backend
            .ensure_installed(&scanner, sender)
            .await
            .unwrap_err();

        let steps = drain(&mut receiver);
        assert_eq!(installed_packages(&steps), ["brscan5"]);
        assert_eq!(
            steps.last(),
            Some(&PairingProgress::Failed {
                message: error.to_string()
            })
        );
        assert_eq!(
            steps.last().unwrap().pairing_state(),
            Some(PairingState::Failed(error.to_string()))
        );
        // A failed install is not an install.
        assert!(!handle.is_installed(&scanner.id));
    }

    /// The install still runs when nobody is listening for its progress — a
    /// `CancelPairing()` that dropped the receiver must not turn into a lost `Err`.
    #[tokio::test]
    async fn a_dropped_progress_receiver_does_not_abort_the_install() {
        let scanner = sample_scanner();
        let backend = MockBackend::with_scanners([scanner.clone()]);
        let handle = backend.handle();
        handle.set_install_packages(["brscan5"]);

        let (sender, receiver) = mpsc::channel(8);
        drop(receiver);

        backend.ensure_installed(&scanner, sender).await.unwrap();
        assert!(handle.is_installed(&scanner.id));
    }

    /// Checklist: dropping the future leaves nothing the next call cannot redo.
    #[tokio::test]
    async fn a_cancelled_install_leaves_nothing_half_done() {
        let scanner = sample_scanner();
        let backend = MockBackend::with_scanners([scanner.clone()]);
        let handle = backend.handle();
        handle.set_install_packages(["brscan5", "brscan-skey"]);

        // Capacity 1 and nobody draining: the future parks on its second send, which is
        // where CancelPairing() would find it. No timer, no sleep.
        let (sender, mut receiver) = mpsc::channel(1);
        let mut installing = Box::pin(backend.ensure_installed(&scanner, sender));
        assert!(poll!(&mut installing).is_pending());
        drop(installing);

        assert!(matches!(
            drain(&mut receiver).as_slice(),
            [PairingProgress::Checking { .. }]
        ));
        assert!(!handle.is_installed(&scanner.id));

        // The retry does the whole job, not the remainder of a half-done one.
        let (sender, mut receiver) = mpsc::channel(8);
        backend.ensure_installed(&scanner, sender).await.unwrap();
        assert_eq!(
            installed_packages(&drain(&mut receiver)),
            ["brscan5", "brscan-skey"]
        );
        assert!(handle.is_installed(&scanner.id));
        assert_eq!(handle.install_attempts(), 2);
    }

    /// Checklist: `fetch_pages` is callable exactly once per job id.
    #[tokio::test]
    async fn fetch_pages_answers_once_per_job() {
        let scanner = sample_scanner();
        let backend = MockBackend::with_scanners([scanner.clone()]);
        let handle = backend.handle();
        handle.feed_pages("job-1", [page(0)]);

        let pages: Vec<Result<RawPage, BackendError>> = backend
            .fetch_pages(&scanner.id, "job-1")
            .await
            .unwrap()
            .collect()
            .await;
        assert_eq!(pages.len(), 1);

        let expected = BackendError::UnknownJob {
            scanner: scanner.id.clone(),
            job: "job-1".to_owned(),
        };
        assert_eq!(
            backend.fetch_pages(&scanner.id, "job-1").await.err(),
            Some(expected)
        );
        // A job that never existed is the same error, deliberately.
        assert!(backend.fetch_pages(&scanner.id, "job-2").await.is_err());
    }

    /// A transfer that dies half way through says so, rather than ending like a batch
    /// that ran out of sheets — the distinction `Job1.State` is built on.
    #[tokio::test]
    async fn a_transfer_that_fails_yields_an_error_item_and_then_ends() {
        let scanner = sample_scanner();
        let backend = MockBackend::with_scanners([scanner.clone()]);
        let handle = backend.handle();
        let lost = BackendError::NotReachable {
            scanner: scanner.id.clone(),
            detail: "the device stopped answering".to_owned(),
        };
        handle.feed_pages_then_fail("job-1", [page(0), page(1)], lost.clone());

        let items: Vec<Result<RawPage, BackendError>> = backend
            .fetch_pages(&scanner.id, "job-1")
            .await
            .unwrap()
            .collect()
            .await;

        assert_eq!(items.len(), 3);
        assert!(items[0].is_ok() && items[1].is_ok());
        assert_eq!(items[2].as_ref().err(), Some(&lost));
    }

    /// The stream a test drives: pages arrive when it says so, and the end of the batch
    /// is the feed going away. This is what makes a job observable while it is running.
    #[tokio::test]
    async fn a_driven_feed_delivers_pages_one_at_a_time() {
        let scanner = sample_scanner();
        let backend = MockBackend::with_scanners([scanner.clone()]);
        let feed = backend.handle().open_pages("job-1");

        let mut pages = backend.fetch_pages(&scanner.id, "job-1").await.unwrap();

        feed.page(page(0)).unwrap();
        assert_eq!(pages.next().await.unwrap().unwrap().index, 0);

        // Nothing has been sent, so the stream is pending rather than finished — the
        // property a `PageCount` assertion between two pages depends on.
        assert!(poll!(pages.next()).is_pending());

        feed.page(page(1)).unwrap();
        assert_eq!(pages.next().await.unwrap().unwrap().index, 1);

        feed.end();
        assert!(pages.next().await.is_none());
    }

    /// A feed nobody reads any more complains about the test rather than doing nothing.
    #[tokio::test]
    async fn a_feed_whose_stream_is_gone_says_so() {
        let scanner = sample_scanner();
        let backend = MockBackend::with_scanners([scanner.clone()]);
        let feed = backend.handle().open_pages("job-1");

        let pages = backend.fetch_pages(&scanner.id, "job-1").await.unwrap();
        drop(pages);

        assert_eq!(
            feed.page(page(0)),
            Err(MockError::PagesDropped("job-1".to_owned()))
        );
    }

    /// Writing `Button1.Profile = ""` has to reach the vendor config as a removal.
    #[tokio::test]
    async fn a_cleared_key_is_removed_rather_than_left_behind() {
        let scanner = sample_scanner();
        let backend = MockBackend::with_scanners([scanner.clone()]);
        let handle = backend.handle();
        let options = BTreeMap::from([(
            "output_folder".to_owned(),
            Value::Str("~/Scans/invoices".to_owned()),
        )]);

        backend
            .set_button_mapping(&scanner.id, 2, Some(ProfileKind::Document), &options)
            .await
            .unwrap();
        assert_eq!(
            handle.button_mapping(&scanner.id, 2),
            Some(ButtonMapping {
                profile: ProfileKind::Document,
                options: options.clone(),
            })
        );

        backend
            .set_button_mapping(&scanner.id, 2, None, &BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(handle.button_mapping(&scanner.id, 2), None);
        assert_eq!(
            handle.calls().last(),
            Some(&MockCall::SetButtonMapping {
                scanner: scanner.id.clone(),
                button_index: 2,
                profile: None,
            })
        );
    }

    /// A refused write leaves the config exactly as it was — the property that 2.5's
    /// `Button1.Profile` setter relies on to keep its old value.
    #[tokio::test]
    async fn a_refused_mapping_changes_nothing() {
        let scanner = sample_scanner();
        let backend = MockBackend::with_scanners([scanner.clone()]);
        let handle = backend.handle();

        backend
            .set_button_mapping(&scanner.id, 2, Some(ProfileKind::Image), &BTreeMap::new())
            .await
            .unwrap();

        handle.fail_button_mapping(BackendError::Other(
            "/opt/brother/scanner/brscan5/brsanenetdevice4.cfg is read-only".to_owned(),
        ));
        assert!(
            backend
                .set_button_mapping(
                    &scanner.id,
                    2,
                    Some(ProfileKind::Document),
                    &BTreeMap::new()
                )
                .await
                .is_err()
        );
        assert_eq!(
            handle.button_mapping(&scanner.id, 2).map(|m| m.profile),
            Some(ProfileKind::Image)
        );

        handle.succeed_button_mapping();
        backend
            .set_button_mapping(
                &scanner.id,
                2,
                Some(ProfileKind::Document),
                &BTreeMap::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            handle.button_mapping(&scanner.id, 2).map(|m| m.profile),
            Some(ProfileKind::Document)
        );
    }

    /// A configured discovery failure is a standing condition, not a one-shot.
    #[tokio::test]
    async fn discovery_can_be_made_to_fail_repeatedly() {
        let backend = MockBackend::new();
        let handle = backend.handle();
        handle.fail_discovery(BackendError::Other("no scanimage on PATH".to_owned()));

        assert!(backend.discover().await.is_err());
        assert!(backend.discover().await.is_err());

        handle.set_discovery([sample_scanner()]);
        assert_eq!(backend.discover().await.unwrap().len(), 1);
        assert_eq!(
            handle
                .calls()
                .iter()
                .filter(|call| **call == MockCall::Discover)
                .count(),
            3
        );
    }

    #[test]
    fn the_sample_scanner_is_the_four_key_brother_of_the_api_doc() {
        let scanner = sample_scanner();
        assert_eq!(scanner.capabilities.button_count(), 4);
        assert!(!scanner.capabilities.buttons.label_configurable);
        assert!(scanner.capabilities.supports_adf());
        assert_eq!(MockBackend::new().id(), "mock");
    }

    /// Two backends in one daemon need two names; the state is still per instance.
    #[tokio::test]
    async fn a_renamed_mock_keeps_its_own_scanners() {
        let escl = MockBackend::with_scanners([sample_scanner()]).with_id("escl");
        assert_eq!(escl.id(), "escl");
        assert_eq!(escl.discover().await.unwrap().len(), 1);
        assert_eq!(MockBackend::new().discover().await.unwrap().len(), 0);
    }
}
