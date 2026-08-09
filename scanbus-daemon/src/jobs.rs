//! [`JobRegistry`]: what a trigger becomes, and how long the object outlives the scan.
//!
//! This is the other half of [`crate::listeners`]. That module answers "who owns the
//! button stream"; this one answers "and then what", which §7 of [`scanbus-dbus-api.md`]
//! draws as one arrow — `data received` → `InterfacesAdded (Job, Button=2)` — and which
//! has four decisions inside it.
//!
//! # The profile is stamped, once, at the trigger
//!
//! §4 says `Job1.Profile` is "copied from `Button1.Profile` at trigger time", and the
//! copy is the whole point: a user who reassigns key 2 while a 20-page ADF batch is still
//! feeding must not change what happens to the pages already received. So the resolution
//! — the precedence order of §3, written down once in
//! [`ButtonEvent::profile`](crate::listeners::ButtonEvent::profile) — runs here, at the
//! press, and what reaches [`Job1`] is a value, not a reference to the key.
//!
//! The per-key `ProfileOptions` are copied in the same breath and for the same reason:
//! they are the profile's arguments, and half of a configuration taken before a write and
//! half after it is not a configuration anyone asked for.
//!
//! # The object appears with the first page, not with the trigger
//!
//! §1 and §4 both say a `Job1` is created "when data is received", and that is meant
//! literally: the object is exported when the first [`RawPage`](scanbus_core::RawPage)
//! comes off the backend's stream, with `PageCount=1`. A press that produces no data at
//! all — the vendor tool has nothing to hand over, the device was switched off between
//! the key and the transfer — therefore publishes nothing, and says so in the log rather
//! than leaving a permanently empty job on the bus for a client to time out on.
//!
//! # A stream that fails is not a stream that ended
//!
//! `fetch_pages` yields `Result`s precisely so that these two are distinguishable: an ADF
//! that ran out of sheets ends the stream and moves the job to `"processing"`, while a
//! device that stops answering after page 3 yields one `Err` and lands the job in
//! `"error"` with that message (§4). Without the distinction every failed transfer would
//! read as a successful short scan.
//!
//! # The object outlives the job, briefly
//!
//! §1 leaves it open whether a finished job is "destroyed or kept in a short history".
//! It is destroyed, after [`JobRegistry::RETENTION`] — 60 seconds. The reason a window is
//! needed at all is that `Result` is only filled at the very end: a client that reacts to
//! `State="done"` and then calls `Get` would be racing the unexport, and `scanbus job
//! watch --until-done` ([`scanbus-cli.md`] §11.3) could not observe the terminal state of
//! a job that finished between two of its events. The reason the window is short is that
//! a history is a second source of truth about jobs, and `GetManagedObjects` is already
//! the first.
//!
//! # Where the profile pipeline goes
//!
//! [`run_profile`] is a hole. [3.1] is what fills it, with `Profile1` objects and the
//! `ProfileProcessor` seam; until then a job with a profile assigned still moves
//! `receiving` → `processing` → `done`, and its `Result` stays empty. The pages are
//! counted and dropped rather than buffered, because there is nothing yet to hand them
//! to and an ADF batch held in memory to be thrown away is megabytes per page of nothing.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md
//! [3.1]: https://github.com/jeanparpaillon/scanbus_service/issues/13

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt as _;
use scanbus_core::{BackendError, JobState, ProfileKind, ScannerBackend, ScannerId, Value};
use tracing::{debug, info, instrument, warn};
use zbus::object_server::InterfaceRef;
use zbus::zvariant::OwnedObjectPath;

use crate::dbus::button::Button1;
use crate::dbus::job::{self, HOST_TRIGGERED, Job1};
use crate::dbus::objects::ObjectRegistry;
use crate::dbus::path;
use crate::listeners::{ButtonEvent, ButtonEventSink};

/// What started a scan — the `Button` property of §4, before it is narrowed to an `i`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobTrigger {
    /// An entry of the device's physical menu, by 0-based index.
    Button(u32),
    /// The host asked for it rather than the device: §3's `Scan()`, and 9.5's upload.
    Host,
}

impl JobTrigger {
    /// The value `Job1.Button` carries: the index, or `-1` for a host-driven scan.
    ///
    /// The saturation is unreachable for real hardware — `buttons.count` is a handful on
    /// every device this daemon can enumerate — and is still not a wrap: `-1` is a
    /// documented value, so an index that wrapped onto it would tell a client the scan
    /// came from the host.
    pub fn button_property(self) -> i32 {
        match self {
            Self::Button(index) => i32::try_from(index).unwrap_or(i32::MAX),
            Self::Host => HOST_TRIGGERED,
        }
    }
}

/// Turns triggers into `Job1` objects, and takes them away again.
///
/// It is the [`ButtonEventSink`] a daemon runs with: [`ScannerRegistry::new`] builds one,
/// and a test that wants to observe the retention window without waiting a minute builds
/// its own with [`JobRegistry::with_retention`] and passes it to
/// [`ScannerRegistry::with_listeners`].
///
/// [`ScannerRegistry::new`]: crate::scanners::ScannerRegistry::new
/// [`ScannerRegistry::with_listeners`]: crate::scanners::ScannerRegistry::with_listeners
pub struct JobRegistry {
    objects: Arc<ObjectRegistry>,
    /// The next job id. Monotonic across the whole daemon rather than per scanner, so a
    /// short id a user reads off `job list` names one job (`scanbus-cli.md` §5) instead
    /// of one per device, and so "not reused within a daemon lifetime" holds without a
    /// second map to consult.
    next_id: AtomicU64,
    retention: Duration,
}

impl JobRegistry {
    /// How long a finished job's object stays on the bus.
    ///
    /// The `60 seconds` [`scanbus-cli.md`] §11.3 asks for, and the argument for a
    /// specific number is there: without a defined window, `job list` is useless and
    /// `job watch --until-done` can miss the terminal state of a job that completed
    /// between two of its events.
    ///
    /// [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md
    pub const RETENTION: Duration = Duration::from_secs(60);

    /// The first job id this daemon mints.
    ///
    /// One rather than zero, because the id is what a user types (`scanbus job show 3`)
    /// and what a path ends in; a `job/0` reads like a placeholder.
    pub const FIRST_ID: u64 = 1;

    /// A registry publishing into `objects`, keeping finished jobs for
    /// [`JobRegistry::RETENTION`].
    pub fn new(objects: Arc<ObjectRegistry>) -> Self {
        Self::with_retention(objects, Self::RETENTION)
    }

    /// The same, with the retention window chosen.
    ///
    /// Exists for the one thing a test needs and a daemon does not: an object that is
    /// still there right after `State="done"` and gone shortly afterwards, without a
    /// minute of `sleep` in the suite.
    pub fn with_retention(objects: Arc<ObjectRegistry>, retention: Duration) -> Self {
        Self {
            objects,
            next_id: AtomicU64::new(Self::FIRST_ID),
            retention,
        }
    }

    /// How long this registry keeps a finished job's object.
    pub fn retention(&self) -> Duration {
        self.retention
    }

    /// Runs one scan through its whole lifecycle, and returns the job id if an object was
    /// published.
    ///
    /// `None` means the trigger produced no data at all — see the module documentation on
    /// why that publishes nothing.
    ///
    /// `profile` and `options` are already resolved: this is the value stamped onto the
    /// object, not something to look up later.
    ///
    /// # Cancellation
    ///
    /// The work runs in a task of its own and this awaits it, so a caller that is
    /// *aborted* mid-scan — a `Disconnect()` while an ADF batch is feeding aborts the
    /// listener task, and that task is what awaits this — detaches the job rather than
    /// killing it. A job that vanished mid-transfer would leave an object stuck in
    /// `"receiving"` with nothing left to move it, which is precisely the registered
    /// object 2.6 must not leave behind.
    pub async fn start(
        &self,
        backend: Arc<dyn ScannerBackend>,
        scanner: ScannerId,
        trigger: JobTrigger,
        profile: Option<ProfileKind>,
        options: BTreeMap<String, Value>,
    ) -> Option<u64> {
        let job_id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let task = tokio::spawn(run(
            Arc::clone(&self.objects),
            backend,
            scanner,
            job_id,
            trigger,
            profile,
            options,
            self.retention,
        ));

        // A `JoinHandle` dropped by an aborted caller *detaches* its task; it does not
        // abort it. That is what makes the cancellation note above true.
        task.await.ok().flatten()
    }

    /// The profile and options assigned to one key, read at the moment of the press.
    ///
    /// A key with no object yields `(None, empty)` rather than failing the job: a backend
    /// that reports fewer buttons than the device has, or an object that went away
    /// between the press and this, still leaves a scan that can be delivered raw — which
    /// is what an unconfigured key produces anyway.
    async fn assignment(
        &self,
        scanner: &ScannerId,
        index: u32,
    ) -> (Option<ProfileKind>, BTreeMap<String, Value>) {
        let path = path::button(scanner, index);

        match self.objects.interface::<Button1>(&path).await {
            Ok(iface) => {
                let key = iface.get().await;
                (key.assigned_profile().await, key.assigned_options().await)
            }
            Err(error) => {
                debug!(%scanner, index, %error, "no button object to read an assignment off");
                (None, BTreeMap::new())
            }
        }
    }
}

#[async_trait]
impl ButtonEventSink for JobRegistry {
    async fn button_pressed(&self, backend: Arc<dyn ScannerBackend>, event: ButtonEvent) {
        let scanner = event.scanner().clone();
        let index = event.button_index();

        // Both halves of the key's configuration, under one read, before anything else
        // can rewrite it — the "copied at trigger time" of §4.
        let (button_profile, options) = self.assignment(&scanner, index).await;
        let profile = event.profile(button_profile);

        self.start(
            backend,
            scanner,
            JobTrigger::Button(index),
            profile,
            options,
        )
        .await;
    }
}

/// The job itself: fetch, count, process, announce, and eventually disappear.
///
/// A free function rather than a method so that it can be spawned: it borrows nothing.
#[expect(
    clippy::too_many_arguments,
    reason = "everything a job needs is owned by it; grouping them into a struct would \
              only move the same list one line up"
)]
#[instrument(level = "info", skip_all, fields(scanner = %scanner, job = job_id))]
async fn run(
    objects: Arc<ObjectRegistry>,
    backend: Arc<dyn ScannerBackend>,
    scanner: ScannerId,
    job_id: u64,
    trigger: JobTrigger,
    profile: Option<ProfileKind>,
    options: BTreeMap<String, Value>,
    retention: Duration,
) -> Option<u64> {
    let path = path::job(&scanner, job_id);
    // The backend is asked for the id *we* minted: `fetch_pages` answers for the daemon's
    // job id rather than inventing one of its own.
    let mut pages = match backend.fetch_pages(&scanner, &job_id.to_string()).await {
        Ok(pages) => pages,
        Err(error) => {
            warn!(%error, "the scan produced no data; no job object was published");
            return None;
        }
    };

    let mut iface: Option<InterfaceRef<Job1>> = None;
    let mut count = 0u32;
    let mut failure: Option<BackendError> = None;

    while let Some(item) = pages.next().await {
        let Ok(_page) = item else {
            // One `Err` ends the transfer; nothing is expected after it.
            failure = item.err();
            break;
        };

        // The page itself is dropped: there is no `ProfileProcessor` to hand it to yet
        // (3.1), and what the object needs is the count.
        count = count.saturating_add(1);

        match &iface {
            // "Created when data is received" (§1): this is the first page.
            None => match publish(&objects, &path, &scanner, trigger, profile, count).await {
                Some(published) => iface = Some(published),
                None => return None,
            },
            Some(iface) => {
                if let Err(error) = job::set_page_count(iface, count).await {
                    warn!(%error, pages = count, "could not announce a page");
                }
            }
        }
    }

    let Some(iface) = iface else {
        match failure {
            Some(error) => {
                warn!(%error, "the transfer failed before any page arrived; no job object")
            }
            None => info!("the scan delivered no pages; no job object was published"),
        }
        return None;
    };

    let (state, result) = match failure {
        Some(error) => (JobState::Error(error.to_string()), BTreeMap::new()),
        None => {
            // End of capture, start of post-processing (§9).
            announce(&iface, JobState::Processing, BTreeMap::new()).await;

            match run_profile(profile, &options).await {
                Ok(result) => (JobState::Done, result),
                Err(message) => (JobState::Error(message), BTreeMap::new()),
            }
        }
    };

    let failed = matches!(state, JobState::Error(_));
    announce(&iface, state, result).await;
    // Dropped before the object is scheduled for removal: holding an interface handle
    // across the retention window would keep the interface alive past its unexport.
    drop(iface);

    if failed {
        warn!(pages = count, "job failed");
    } else {
        info!(pages = count, "job done");
    }

    retire(objects, path, retention);

    Some(job_id)
}

/// Exports the job object, with the first page already counted.
///
/// `None` when the export failed, which ends the job: an object the bus does not serve is
/// one no client can be told anything about, and going on would spend the whole transfer
/// emitting signals into nothing.
async fn publish(
    objects: &Arc<ObjectRegistry>,
    path: &OwnedObjectPath,
    scanner: &ScannerId,
    trigger: JobTrigger,
    profile: Option<ProfileKind>,
    count: u32,
) -> Option<InterfaceRef<Job1>> {
    let job = Job1::new(
        path::scanner(scanner),
        trigger.button_property(),
        profile,
        count,
    );

    if let Err(error) = objects.add(path.clone(), job).await {
        warn!(%error, "could not publish the job object; the scan is abandoned");
        return None;
    }

    info!(
        button = trigger.button_property(),
        profile = ProfileKind::optional_as_str(profile),
        "job started"
    );

    match objects.interface::<Job1>(path).await {
        Ok(iface) => Some(iface),
        Err(error) => {
            warn!(%error, "the job object went away as it was published");
            None
        }
    }
}

/// Moves the job's state, logging an emission that did not go out.
///
/// Not fatal: the daemon's own view has already moved, and a client that missed the
/// signal reads the property. What it must not do is stop the job.
async fn announce(iface: &InterfaceRef<Job1>, state: JobState, result: BTreeMap<String, Value>) {
    if let Err(error) = job::transition(iface, state, result).await {
        warn!(%error, "could not announce a job transition");
    }
}

/// Schedules the object's removal once the retention window is up.
///
/// Spawned rather than awaited, because the caller is what serialises presses on one
/// scanner ([`crate::listeners`]): waiting out the window here would make the *next*
/// press wait a minute for a job that has already finished.
fn retire(objects: Arc<ObjectRegistry>, path: OwnedObjectPath, retention: Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(retention).await;

        match objects.remove_object(&path).await {
            Ok(()) => debug!(%path, "job object removed after the retention window"),
            // The scanner went first — `Unpair()`, the end of a discovery session, or
            // shutdown — and took its subtree with it. Nothing left to remove.
            Err(error) => debug!(%path, %error, "the job object had already gone"),
        }
    });
}

/// The profile pipeline, as far as this iteration has one: it has none.
///
/// [3.1] is the issue that replaces this with `Profile1` objects and the
/// `ProfileProcessor` seam of the implementation plan §6, and it is also the issue that
/// decides how the pages reach a processor — as a stream it drives itself, or as the
/// buffer `DocumentProcessor` needs for PDF assembly. Deciding that here, with nothing to
/// consume either shape, would be inventing the seam that issue exists to design.
///
/// Until then the job still runs its whole lifecycle: `Result` stays empty, which §6
/// makes readable — every documented shape is a map, and an empty one is "nothing was
/// produced" rather than a malformed reply.
///
/// [3.1]: https://github.com/jeanparpaillon/scanbus_service/issues/13
async fn run_profile(
    profile: Option<ProfileKind>,
    options: &BTreeMap<String, Value>,
) -> Result<BTreeMap<String, Value>, String> {
    debug!(
        profile = ProfileKind::optional_as_str(profile),
        options = options.len(),
        "no profile processor yet (3.1); the job finishes with an empty Result"
    );

    Ok(BTreeMap::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §4's `Button`: the index for a key, `-1` for anything the host started.
    #[test]
    fn a_host_driven_job_is_the_only_one_that_reports_minus_one() {
        assert_eq!(JobTrigger::Button(0).button_property(), 0);
        assert_eq!(JobTrigger::Button(3).button_property(), 3);
        assert_eq!(JobTrigger::Host.button_property(), HOST_TRIGGERED);

        // An index no device has, and still not a `-1` that would misreport the trigger.
        assert_eq!(JobTrigger::Button(u32::MAX).button_property(), i32::MAX);
    }

    /// The hole 3.1 fills: a lifecycle that completes, with nothing in `Result`.
    #[tokio::test]
    async fn the_profile_pipeline_is_a_hole_that_still_finishes() {
        let result = run_profile(Some(ProfileKind::Document), &BTreeMap::new())
            .await
            .expect("a missing processor is not a failed job");

        assert!(result.is_empty());
    }
}
