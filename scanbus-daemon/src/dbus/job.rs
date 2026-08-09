//! [`Job1`]: one scan in flight, and the three fields that are stamped and never re-read.
//!
//! §4 of [`scanbus-dbus-api.md`] makes a job an *object* rather than a return value, and
//! §1 gives it a lifetime bounded by the scan: it appears when data starts arriving and
//! goes away once post-processing is finished. That is what lets a desktop UI which was
//! not running when the user walked up to the device still see the scan — it subscribes
//! to the manager, and `GetManagedObjects` already contains the job (§4).
//!
//! # `Scanner`, `Button` and `Profile` are copied at trigger time
//!
//! All three are `emits_changed_signal = "const"`, because they are decided once, when
//! the job is created, and never revised. §4 says so of `Profile` in as many words —
//! "copied from `Button1.Profile` at trigger time" — and the reason is the case a lazy
//! read would break: a user reassigns key 2 from `document` to `image` while a 20-page
//! ADF batch is still feeding, and the pages already received would suddenly be destined
//! for a different pipeline. The copy is made by [`crate::jobs`], which also resolves the
//! precedence order of §3 before it gets here.
//!
//! `Button` is an `i` and not a `u` for the same reason it carries `-1`: a job that no
//! physical key started — a host-driven scan — has to be expressible, and a wrapped
//! `u32::MAX` is not a value a client can branch on.
//!
//! # `State` and `Error` are one thing, and are emitted as one signal
//!
//! The pair has an invariant D-Bus cannot express — a non-empty `Error` alongside
//! `State="done"` is nonsense — so internally they are a single [`JobState`], whose
//! `Error` variant carries the message. What reaches the bus is one `PropertiesChanged`
//! per transition carrying `State` *and* whichever of `Result`/`Error` moved with it
//! ([`transition`]), rather than two or three signals a client would have to correlate:
//! §4's whole point is that a client following `PropertiesChanged` sees the outcome, and
//! a client that read `State="done"` and then had to `Get` `Result` would be racing the
//! retention window.
//!
//! # Every method takes `&self`
//!
//! Same rule, and the same reason, as [`crate::dbus::scanner`]: zbus takes a write lock
//! on the interface for a `&mut self` method or a
//! [`get_mut`](zbus::object_server::InterfaceRef::get_mut), and the registry emits
//! property changes while holding locks of its own. The mutable state therefore lives
//! behind this type's [`Mutex`]es, and the free functions below are how it moves.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use scanbus_core::{JobState, ProfileKind, Value};
use tracing::debug;
use zbus::names::InterfaceName;
use zbus::object_server::InterfaceRef;
use zbus::zvariant::{OwnedObjectPath, Value as ZValue};

use crate::dbus::convert::{self, Dict};

/// The interface name, spelled once: [`transition`] builds its own `PropertiesChanged`.
pub const INTERFACE: &str = "org.scanbus.Job1";

/// The `Button` value for a scan the host asked for rather than the device (§4).
pub const HOST_TRIGGERED: i32 = -1;

/// The `org.scanbus.Job1` object of §4, at `/org/scanbus/scanner/{id}/job/{jobid}`.
///
/// Created by [`crate::jobs`] on the first page and removed by it after the retention
/// window; nothing else exports or mutates one.
pub struct Job1 {
    /// Path of the `Scanner1` this job came from.
    scanner: OwnedObjectPath,
    /// The key that started it, or [`HOST_TRIGGERED`].
    button: i32,
    /// The profile this job runs, resolved once at creation. `None` is §4's `""`: the
    /// data is delivered raw.
    profile: Option<ProfileKind>,
    /// Where the job has got to, with the failure message inside the terminal variant.
    state: Mutex<JobState>,
    /// Pages received so far — 1 as soon as the object exists, since the first page is
    /// what publishes it.
    page_count: Mutex<u32>,
    /// The profile pipeline's outcome, empty until `State="done"` (§6).
    result: Mutex<BTreeMap<String, Value>>,
}

impl Job1 {
    /// A job that has just received its first page.
    ///
    /// `page_count` is a parameter rather than a constant so the caller states what it
    /// has actually seen; it is `1` on every path today, and the object does not exist
    /// before there is a page to count.
    pub fn new(
        scanner: OwnedObjectPath,
        button: i32,
        profile: Option<ProfileKind>,
        page_count: u32,
    ) -> Self {
        Self {
            scanner,
            button,
            profile,
            state: Mutex::new(JobState::Receiving),
            page_count: Mutex::new(page_count),
            result: Mutex::new(BTreeMap::new()),
        }
    }

    /// Where the job has got to, as a [`JobState`] rather than as the two properties §4
    /// splits it over.
    ///
    /// Named apart from the `State` getter below, which the interface macro owns.
    pub fn job_state(&self) -> JobState {
        self.state.lock().expect("job state lock poisoned").clone()
    }

    /// How many pages have arrived.
    ///
    /// Named apart from the `PageCount` getter below, which the interface macro owns.
    pub fn pages(&self) -> u32 {
        *self.page_count.lock().expect("page count lock poisoned")
    }

    /// The profile this job was stamped with, `None` for raw.
    ///
    /// Named apart from the `Profile` getter below, which the interface macro owns.
    pub fn assigned_profile(&self) -> Option<ProfileKind> {
        self.profile
    }
}

#[zbus::interface(name = "org.scanbus.Job1")]
impl Job1 {
    /// Path of the `Scanner1` object this job came from.
    ///
    /// `const`: a job belongs to the scanner whose path it hangs off, so a change in it
    /// would be a different object.
    #[zbus(property(emits_changed_signal = "const"))]
    fn scanner(&self) -> OwnedObjectPath {
        self.scanner.clone()
    }

    /// Index of the physical key that started this job, `-1` for a host-driven scan (§4).
    ///
    /// `const`: copied at trigger time — see the module documentation.
    #[zbus(property(emits_changed_signal = "const"))]
    fn button(&self) -> i32 {
        self.button
    }

    /// The profile being applied, `""` for raw.
    ///
    /// `const`: copied from `Button1.Profile` at trigger time (§4), so reassigning the
    /// key mid-scan does not change what happens to the pages already received.
    #[zbus(property(emits_changed_signal = "const"))]
    fn profile(&self) -> String {
        ProfileKind::optional_as_str(self.profile).to_owned()
    }

    /// `"receiving"` → `"processing"` → `"done"` / `"error"` (§4).
    ///
    /// The job stays `"receiving"` for as long as the backend reports further pages;
    /// `"processing"` marks the end of capture and the start of the profile pipeline
    /// (§9).
    #[zbus(property)]
    fn state(&self) -> String {
        self.job_state().as_str().to_owned()
    }

    /// Pages received so far — what a UI shows during a 20-page ADF run.
    #[zbus(property)]
    fn page_count(&self) -> u32 {
        self.pages()
    }

    /// The profile pipeline's outcome, profile-specific (§6).
    ///
    /// Empty until `State="done"`, and empty after it too for as long as there is no
    /// `ProfileProcessor` to fill it — see [`crate::jobs`].
    #[zbus(property)]
    fn result(&self) -> Dict {
        convert::dict(
            self.result
                .lock()
                .expect("job result lock poisoned")
                .iter()
                .map(|(key, value)| (key.clone(), value)),
        )
    }

    /// Why the job failed, empty unless `State="error"`.
    #[zbus(property)]
    fn error(&self) -> String {
        self.job_state().error().to_owned()
    }
}

/// Sets `PageCount` and announces it, if it moved.
///
/// Set rather than incremented: the job runner counts the pages it has taken off the
/// stream, and a second counter here is one more thing that can disagree with it — which
/// is exactly what would happen the first time an emission failed and the caller retried.
///
/// # Errors
///
/// Whatever zbus failed to emit. The count has already moved in that case, which is why
/// the caller logs rather than swallows it: a client that missed the signal reads a stale
/// `PageCount` until it polls.
pub async fn set_page_count(iface: &InterfaceRef<Job1>, count: u32) -> zbus::Result<()> {
    // `get`, never `get_mut`: see the module documentation on why this interface is only
    // ever read-locked.
    let job = iface.get().await;

    {
        let mut pages = job.page_count.lock().expect("page count lock poisoned");
        if *pages == count {
            return Ok(());
        }
        *pages = count;
    }

    job.page_count_changed(iface.signal_emitter()).await
}

/// Moves the job to `state`, with `result` when it is [`JobState::Done`], and announces
/// every property that moved in one signal.
///
/// One signal rather than two or three because a client reacting to `State="done"` has to
/// be able to read `Result` out of the same message — see the module documentation.
/// Emitting nothing when the state has not actually moved keeps a repeated terminal
/// transition (an error path that runs twice) from looking like a second job outcome.
///
/// # Errors
///
/// Whatever zbus failed to emit; the in-memory state has already moved.
pub async fn transition(
    iface: &InterfaceRef<Job1>,
    state: JobState,
    result: BTreeMap<String, Value>,
) -> zbus::Result<()> {
    let job = iface.get().await;

    let previous = {
        let mut current = job.state.lock().expect("job state lock poisoned");
        if *current == state {
            return Ok(());
        }
        std::mem::replace(&mut *current, state.clone())
    };

    let mut changed: HashMap<&str, ZValue<'_>> = HashMap::new();

    if previous.as_str() != state.as_str() {
        changed.insert("State", ZValue::from(state.as_str()));
    }
    // Its own comparison, because `error → error` with a different message is a real
    // change to this property and `receiving → processing` is not one.
    if previous.error() != state.error() {
        changed.insert("Error", ZValue::from(state.error()));
    }

    if !result.is_empty() {
        *job.result.lock().expect("job result lock poisoned") = result;
        changed.insert("Result", ZValue::from(job.result()));
    }

    debug!(from = %previous, to = %state, "job state changed");

    if changed.is_empty() {
        return Ok(());
    }

    zbus::fdo::Properties::properties_changed(
        iface.signal_emitter(),
        InterfaceName::from_static_str_unchecked(INTERFACE),
        changed,
        Cow::Borrowed(&[]),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(profile: Option<ProfileKind>) -> Job1 {
        Job1::new(
            OwnedObjectPath::try_from("/org/scanbus/scanner/mock_usb_3A001_3A002").unwrap(),
            2,
            profile,
            1,
        )
    }

    /// The three constant properties, including §4's `""` for a job with no profile.
    #[test]
    fn the_stamped_properties_read_as_documented() {
        let stamped = job(Some(ProfileKind::Document));
        assert_eq!(
            stamped.scanner().as_str(),
            "/org/scanbus/scanner/mock_usb_3A001_3A002"
        );
        assert_eq!(stamped.button(), 2);
        assert_eq!(stamped.profile(), "document");

        assert_eq!(job(None).profile(), "", "an unassigned key delivers raw");
    }

    /// A fresh job is receiving, one page in, with nothing to report either way.
    #[test]
    fn a_new_job_is_receiving_its_first_page() {
        let job = job(None);

        assert_eq!(job.state(), "receiving");
        assert_eq!(job.page_count(), 1);
        assert_eq!(job.error(), "");
        assert!(job.result().is_empty());
    }

    /// `Error` is non-empty exactly when the job failed — the invariant [`JobState`]
    /// holds by construction, checked through the properties that render it.
    #[test]
    fn the_error_property_follows_the_state() {
        let job = job(None);

        for state in [
            JobState::Receiving,
            JobState::Processing,
            JobState::Done,
            JobState::Error("the device stopped answering".to_owned()),
        ] {
            *job.state.lock().unwrap() = state.clone();
            assert_eq!(job.state(), state.as_str());
            assert_eq!(
                !job.error().is_empty(),
                matches!(state, JobState::Error(_)),
                "{state:?} rendered Error={:?}",
                job.error()
            );
        }
    }

    /// `Button = -1` is the host-driven scan of §4, and survives as an `i32` rather than
    /// wrapping the way a `u32` index would.
    #[test]
    fn a_host_driven_job_reports_minus_one() {
        let job = Job1::new(
            OwnedObjectPath::try_from("/org/scanbus/scanner/mock_usb").unwrap(),
            HOST_TRIGGERED,
            Some(ProfileKind::Image),
            1,
        );

        assert_eq!(job.button(), -1);
    }
}
