//! Who owns the button stream, what it is turned into, and what happens when it ends.
//!
//! §3 of [`scanbus-dbus-api.md`] says `Connect()` "declares the host ready to receive".
//! In implementation terms that is a task consuming the
//! [`ButtonPressedEvent`](scanbus_core::ButtonPressedEvent) stream a backend hands back
//! from [`start_listening`](scanbus_core::ScannerBackend::start_listening). The awkward
//! part is not starting it — it is that *two* pieces of the daemon would otherwise
//! believe they own it, because [`PairingMachine`](scanbus_core::PairingMachine) starts
//! listening as the last step of pairing (1.4) and then drops the stream again.
//!
//! # One owner: the registry
//!
//! [`ScannerRegistry`] owns one listener task per scanner, keyed by
//! [`ScannerId`](scanbus_core::ScannerId), and everything that wants one asks it to
//! *ensure* the task exists rather than to start one:
//!
//! - `Connect()` — [`ScannerRegistry::ensure_listening`];
//! - the pairing supervisor, on the transition to `done`
//!   ([`crate::dbus::scanner::supervise`]);
//! - the restore path at startup (4.2), which is the same call and not a parallel one.
//!
//! `Connect()` is therefore idempotent for free: the second call finds a live task and
//! changes nothing, which is why the `Connected` property can be "a listener task is
//! running" and not a flag someone has to remember to clear.
//!
//! # A stream that ends is a lost scan, so it is retried
//!
//! # A press is a whole scan, and the listener waits for it
//!
//! [`ButtonEventSink::button_pressed`] is awaited, and the sink a daemon runs with
//! ([`JobRegistry`](crate::jobs::JobRegistry)) does not return from it until the scan has
//! finished. That is deliberate: a device scans one thing at a time, so the second press
//! of a double-tap has to wait for the first job rather than race it onto the same
//! hardware. What it must not do is die with this task — see the trait's own note.
//!
//! The listener is the only thing between a physical key and a `Job1`. A stream that
//! ends because the vendor daemon died — the likely case for `brscan-skey`, see [5.3] —
//! must not leave a scanner sitting at `Connected=true` that will never produce a job
//! again. So the task restarts it with bounded exponential backoff
//! ([`RestartPolicy`]), and when the budget is exhausted it says so where a client can
//! see it: `Status="error"` and `Connected=false`, not just a log line.
//!
//! The attempt counter is reset by a *press*, not by a successful `start_listening`: a
//! vendor daemon that accepts a connection and drops it again immediately would
//! otherwise be retried forever.
//!
//! # Presses are queued, never dropped
//!
//! Events are handed to the [`ButtonEventSink`] one at a time, in order, and the task
//! awaits each one. A press that arrives while the previous one is still being turned
//! into a job therefore waits — it is not rejected and not dropped. That is the choice
//! the checklist of 2.4 asks to be explicit about, and it is the one that matches what a
//! user did: they pressed the key twice, and both presses are theirs. The queue is
//! bounded by the backend's own stream buffer, so a sink that stalls indefinitely
//! becomes backpressure the backend reports (for
//! [`MockBackend`](scanbus_core::backend::mock::MockBackend), a `ListenerFull` error at
//! the point of the press) rather than unbounded growth here.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md
//! [5.3]: https://github.com/jeanparpaillon/scanbus_service/issues/20

use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use futures_util::StreamExt as _;
use futures_util::stream::BoxStream;
use scanbus_core::{
    BackendError, ButtonPressedEvent, ProfileKind, ScannerBackend, ScannerId, ScannerInfo,
};
use thiserror::Error;
use tracing::{debug, info, instrument, warn};
use zbus::zvariant::OwnedObjectPath;

use crate::dbus::objects::ObjectRegistry;
use crate::dbus::path;
use crate::dbus::scanner::Scanner1;
use crate::scanners::ScannerRegistry;

/// A press, with the two profiles the host had configured when it arrived.
///
/// The press itself carries no profile — a backend that reported one would be reading
/// back a config file we wrote (see
/// [`ButtonPressedEvent`](scanbus_core::ButtonPressedEvent)) — so the profile is resolved
/// on this side, by [`ButtonEvent::profile`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ButtonEvent {
    /// What the backend observed.
    pub press: ButtonPressedEvent,
    /// The `{"profile": …}` of the `Connect()` that opened this session, if any.
    pub session_profile: Option<ProfileKind>,
    /// `Scanner1.DefaultProfile` as it stood when the press arrived.
    pub default_profile: Option<ProfileKind>,
}

impl ButtonEvent {
    /// The profile a job for this press must be stamped with — the precedence order of
    /// §3, written down once.
    ///
    /// ```text
    /// Button1.Profile  >  session profile  >  Scanner1.DefaultProfile
    /// ```
    ///
    /// From most specific to least: the key the user configured for *this* button, then
    /// the override the client asked for when it connected, then the scanner-wide
    /// fallback. `None` all the way down is §4's `Profile=""`, i.e. deliver the data raw.
    ///
    /// `button_profile` is a parameter rather than a field because the `Button1` objects
    /// that hold it are [2.5]'s; [2.6] passes what it reads off the matching object at
    /// the moment it stamps the job, which is the same "read it now, not when the
    /// listener started" rule the API's §7 flow describes.
    ///
    /// [2.5]: https://github.com/jeanparpaillon/scanbus_service/issues/9
    /// [2.6]: https://github.com/jeanparpaillon/scanbus_service/issues/10
    pub fn profile(&self, button_profile: Option<ProfileKind>) -> Option<ProfileKind> {
        button_profile
            .or(self.session_profile)
            .or(self.default_profile)
    }

    /// The scanner the press came from.
    pub fn scanner(&self) -> &ScannerId {
        &self.press.scanner_id
    }

    /// Position in the physical menu, 0-based.
    pub fn button_index(&self) -> u32 {
        self.press.button_index
    }

    /// When the backend observed the press.
    pub fn timestamp(&self) -> SystemTime {
        self.press.timestamp
    }
}

/// What a button press is turned into.
///
/// A seam rather than a direct call into [`crate::jobs`], for two reasons that survived
/// 2.6: the job side needs an object registry, a job id and a profile pipeline, none of
/// which this module should have to thread through its restart logic — and a test that is
/// about *the listener* (a stream that ends, a budget that runs out) wants to assert that
/// a press arrived without standing a whole scan up behind it.
///
/// `backend` is a parameter rather than a field of [`ButtonEvent`] because it is context
/// and not part of the press: the sink needs it to fetch the pages, and it must be the
/// same instance the listener was started against — not whatever the daemon's backend
/// list happens to rank first by the time the key is hit.
///
/// Implementors must not block: the task awaits this call before reading the next press,
/// which is what makes presses queue rather than interleave (see the module
/// documentation). [`JobRegistry`](crate::jobs::JobRegistry) awaits the whole scan here on
/// purpose — one device scans one thing at a time — and runs it in a task of its own so
/// that aborting the listener detaches the job instead of killing it.
#[async_trait]
pub trait ButtonEventSink: Send + Sync {
    /// One key was pressed on a connected scanner.
    async fn button_pressed(&self, backend: Arc<dyn ScannerBackend>, event: ButtonEvent);
}

/// How hard a listener tries to come back before the scanner is declared broken.
///
/// A field of the registry rather than a constant, because the two callers want
/// different things from it: a daemon wants a budget long enough to outlast a vendor
/// daemon being restarted, and a test wants the exhausted case to be observable without
/// putting half a minute into the suite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    /// Delay before the first retry; doubled for each one after it.
    pub initial: Duration,
    /// Ceiling on that doubling.
    pub max: Duration,
    /// How many restarts to attempt before giving up. `0` disables restarting.
    pub attempts: u32,
}

impl RestartPolicy {
    /// Roughly 45 s of trying, in six attempts.
    ///
    /// Long enough to cover a `brscan-skey` restarted by hand or by a package upgrade,
    /// and short enough that a scanner which is really gone reaches `Status="error"`
    /// while the user is still looking at the screen that told them to press the key.
    pub const DEFAULT: Self = Self {
        initial: Duration::from_millis(250),
        max: Duration::from_secs(30),
        attempts: 6,
    };

    /// The delay before attempt number `attempt`, counted from zero.
    ///
    /// Saturating rather than wrapping: a policy with a large `attempts` must not have
    /// its delay overflow back to something short, which would turn a backoff into a
    /// tight retry loop against a device that is not there.
    pub fn delay(&self, attempt: u32) -> Duration {
        self.initial
            .saturating_mul(1u32.checked_shl(attempt).unwrap_or(u32::MAX))
            .min(self.max)
    }

    /// The worst case total: what "within the backoff budget" means.
    pub fn budget(&self) -> Duration {
        (0..self.attempts)
            .map(|attempt| self.delay(attempt))
            .fold(Duration::ZERO, |total, delay| total.saturating_add(delay))
    }
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Why a listener could not be started.
///
/// Deliberately thin: the interesting failures all come from the backend, and turning
/// them into the named errors of §8 is the caller's job — see
/// [`crate::dbus::scanner`], where `Connect()` does it for the subset it can produce.
#[derive(Debug, Error)]
pub enum ListenError {
    /// The registry has no object for that scanner. Reachable when a discovery session
    /// ends between a client reading a path and calling `Connect()` on it.
    #[error("no scanner object for {0}")]
    Unknown(ScannerId),

    /// The backend refused to start listening.
    #[error(transparent)]
    Backend(#[from] BackendError),
}

/// One scanner's listener task: consume, restart, and eventually give up.
///
/// Spawned by [`ScannerRegistry::ensure_listening`] with the first stream already open,
/// so that a `Connect()` which cannot listen at all fails as a method call rather than
/// as a log line in a task nobody is watching.
///
/// This function never returns while it is healthy. Ending it is
/// [`abort`](tokio::task::JoinHandle::abort) — from `Disconnect()`, `Unpair()` or
/// shutdown — which drops the stream and, for a backend that stops on drop, is already
/// most of the teardown; the registry follows it with an explicit
/// [`stop_listening`](scanbus_core::ScannerBackend::stop_listening) for the backends that
/// need one.
#[instrument(level = "debug", skip_all, fields(id = %scanner.id))]
pub(crate) async fn run(
    scanners: Weak<ScannerRegistry>,
    objects: Arc<ObjectRegistry>,
    backend: Arc<dyn ScannerBackend>,
    scanner: ScannerInfo,
    sink: Arc<dyn ButtonEventSink>,
    policy: RestartPolicy,
    mut stream: BoxStream<'static, ButtonPressedEvent>,
) {
    let path = path::scanner(&scanner.id);
    let mut attempt = 0u32;

    loop {
        while let Some(press) = stream.next().await {
            // A press is the proof that this stream is real, which is what makes it —
            // and not a successful `start_listening` — the thing that clears the budget.
            attempt = 0;

            let (session_profile, default_profile) = profiles(&objects, &path).await;
            debug!(
                button = press.button_index,
                profile = ProfileKind::optional_as_str(session_profile.or(default_profile)),
                "button press received"
            );
            sink.button_pressed(
                Arc::clone(&backend),
                ButtonEvent {
                    press,
                    session_profile,
                    default_profile,
                },
            )
            .await;
        }

        // Reaching here is always unexpected: every deliberate stop aborts this task
        // instead, so a stream that merely ended means the backend lost the device.
        loop {
            if attempt >= policy.attempts {
                warn!(
                    attempts = policy.attempts,
                    "the button stream could not be restored; the scanner is in error"
                );
                if let Some(scanners) = scanners.upgrade() {
                    scanners.listener_failed(&scanner.id).await;
                }
                return;
            }

            let delay = policy.delay(attempt);
            attempt += 1;
            warn!(
                attempt,
                delay_ms = delay.as_millis() as u64,
                "the button stream ended; restarting the listener"
            );
            tokio::time::sleep(delay).await;

            match backend.start_listening(&scanner).await {
                Ok(restarted) => {
                    info!(attempt, "listener restarted");
                    stream = restarted;
                    break;
                }
                Err(error) => warn!(attempt, %error, "could not restart the listener"),
            }
        }
    }
}

/// The session profile and `DefaultProfile` of a scanner, read at the moment of a press.
///
/// Read per press rather than captured when the listener started, because both are
/// writable while a scanner is connected: a client that sets `DefaultProfile` and then
/// presses the key must get the profile it just set.
///
/// An object that has gone away yields `(None, None)` — the press is still delivered,
/// raw, which is the same thing an unconfigured scanner produces.
async fn profiles(
    objects: &ObjectRegistry,
    path: &OwnedObjectPath,
) -> (Option<ProfileKind>, Option<ProfileKind>) {
    match objects.interface::<Scanner1>(path).await {
        Ok(iface) => {
            let scanner = iface.get().await;
            (scanner.session_profile(), scanner.default_profile_value())
        }
        Err(error) => {
            warn!(%path, %error, "no object to read the profiles off; delivering raw");
            (None, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The precedence order of §3, which [2.6] reads.
    #[test]
    fn the_button_wins_over_the_session_which_wins_over_the_default() {
        let event = ButtonEvent {
            press: ButtonPressedEvent::now(
                ScannerId::from_backend("mock", "usb:001:002").unwrap(),
                2,
            ),
            session_profile: Some(ProfileKind::Document),
            default_profile: Some(ProfileKind::Image),
        };

        assert_eq!(
            event.profile(Some(ProfileKind::Ocr)),
            Some(ProfileKind::Ocr)
        );
        assert_eq!(event.profile(None), Some(ProfileKind::Document));

        let no_session = ButtonEvent {
            session_profile: None,
            ..event.clone()
        };
        assert_eq!(no_session.profile(None), Some(ProfileKind::Image));

        let nothing = ButtonEvent {
            session_profile: None,
            default_profile: None,
            ..event
        };
        assert_eq!(nothing.profile(None), None);
    }

    #[test]
    fn the_backoff_doubles_and_then_stops_doubling() {
        let policy = RestartPolicy {
            initial: Duration::from_millis(100),
            max: Duration::from_millis(400),
            attempts: 5,
        };

        let delays: Vec<u64> = (0..5).map(|n| policy.delay(n).as_millis() as u64).collect();
        assert_eq!(delays, [100, 200, 400, 400, 400]);
        assert_eq!(policy.budget(), Duration::from_millis(1500));
    }

    /// A long budget must not overflow its way back to a tight retry loop.
    #[test]
    fn a_far_out_attempt_saturates_at_the_ceiling() {
        let policy = RestartPolicy::DEFAULT;

        assert_eq!(policy.delay(0), Duration::from_millis(250));
        assert_eq!(policy.delay(64), policy.max);
        assert_eq!(policy.delay(u32::MAX), policy.max);
    }

    #[test]
    fn no_attempts_means_no_restart_and_no_budget() {
        let policy = RestartPolicy {
            attempts: 0,
            ..RestartPolicy::DEFAULT
        };

        assert_eq!(policy.budget(), Duration::ZERO);
    }
}
