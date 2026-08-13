//! What a backend reports while it is running: scan triggers, and install progress.

use std::time::SystemTime;

use crate::model::{PairingState, ProfileKind, ScannerId};

/// Identifier the backend minted for one trigger.
pub type TriggerId = String;

/// What kind of thing triggered a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerKind {
    /// One entry of the device's physical menu was selected.
    Button { index: u32 },
    /// The device chose the profile and is already sending the bytes.
    Push { profile: ProfileKind },
}

/// One thing happened that may become a job.
///
/// A physical key deliberately carries **no profile**. The profile lives in
/// `Button1.Profile` on the service side ([`scanbus-dbus-api.md`] §5): a backend that
/// knows only "key 2 was pressed" is complete, and a backend that reported a profile
/// would be telling us something it read back out of a config file *we* wrote — a round
/// trip with two copies of the truth in it.
///
/// A pushed upload does carry a profile, because the host never wrote it: it genuinely
/// originates on the phone.
///
/// Not `Serialize`, for the same reason as [`RawPage`](crate::model::RawPage): an event
/// in flight between a backend and the registry is never persisted and never on the bus
/// in this shape.
///
/// [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanTrigger {
    /// The backend-issued correlation key this transfer will be fetched back with.
    pub id: TriggerId,
    /// The scanner the trigger came from.
    pub scanner_id: ScannerId,
    /// What kind of trigger this is.
    pub kind: TriggerKind,
    /// When the backend observed the press.
    ///
    /// The backend's clock reading, not the registry's: a backend that polls a device
    /// or drains a queue knows better than its caller when the key was actually hit.
    pub timestamp: SystemTime,
}

impl ScanTrigger {
    /// A press observed now.
    pub fn button(scanner_id: ScannerId, id: impl Into<TriggerId>, index: u32) -> Self {
        Self {
            id: id.into(),
            scanner_id,
            kind: TriggerKind::Button { index },
            timestamp: SystemTime::now(),
        }
    }

    /// An upload observed now.
    pub fn push(scanner_id: ScannerId, id: impl Into<TriggerId>, profile: ProfileKind) -> Self {
        Self {
            id: id.into(),
            scanner_id,
            kind: TriggerKind::Push { profile },
            timestamp: SystemTime::now(),
        }
    }
}

/// A step of [`ensure_installed`](super::ScannerBackend::ensure_installed).
///
/// This exists because `Pair()` must return immediately while installing a Brother
/// `.deb` takes tens of seconds ([`scanbus-dbus-api.md`] §9). The channel these travel
/// on is the only thing that tells the pairing state machine (1.4) *when* to move
/// `PairingState` from `"pairing"` to `"installing_backend"` — hence
/// [`PairingProgress::pairing_state`], which is that decision written down once instead
/// of in every backend.
///
/// [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingProgress {
    /// Working out what is already present. Nothing has been changed on the system yet,
    /// which is what makes this step safe to cancel.
    Checking {
        /// What is being checked, in a form worth showing a user.
        message: String,
    },

    /// The backend is working on one named dependency. Maps to
    /// [`PairingState::InstallingBackend`].
    ///
    /// "Installing" names the *phase*, not necessarily an action: a backend that only
    /// verifies its dependencies and refuses when they are missing — which is what the
    /// Brother one does, deliberately — still emits this per package it depends on. The
    /// sequence a client sees is the contract, and it must not change the day a backend
    /// gains the ability to install what it currently only checks.
    Installing {
        /// Package, `.deb` or step, e.g. `"brscan5"`.
        package: String,
        /// Completion in percent, when the packager reports one. `None` is the normal
        /// case: `apt` and `dpkg` do not give a usable fraction.
        percent: Option<u8>,
    },

    /// Every dependency is present. `ensure_installed` returns `Ok` right after this.
    ///
    /// Not the end of pairing: `start_listening` still has to succeed before the state
    /// machine reaches `"done"`, which is why this maps to no state of its own.
    Ready,

    /// Waiting for a human to confirm on the device — a code to compare, a PIN to enter,
    /// a physical button. Deliberately generic: nothing here says "mobile".
    AwaitingConfirmation {
        /// What a client shows next to the device's name, e.g. a six-digit code.
        code: String,
    },

    /// The attempt failed. Carries the same message as the [`BackendError`] the call is
    /// about to return, so a client watching `PairingError` and a caller reading the
    /// `Err` see one story.
    ///
    /// [`BackendError`]: crate::error::BackendError
    Failed {
        /// Why it failed.
        message: String,
    },
}

impl PairingProgress {
    /// The `PairingState` this step implies, or `None` when it implies no transition.
    ///
    /// [`PairingProgress::Ready`] is the `None`: the dependencies are in place but
    /// pairing is not done until the listener is running, so the state stays whatever
    /// it was.
    pub fn pairing_state(&self) -> Option<PairingState> {
        match self {
            Self::Checking { .. } => Some(PairingState::Pairing),
            Self::Installing { .. } => Some(PairingState::InstallingBackend),
            Self::Ready => None,
            Self::AwaitingConfirmation { code } => {
                Some(PairingState::AwaitingConfirmation(code.clone()))
            }
            Self::Failed { message } => Some(PairingState::Failed(message.clone())),
        }
    }

    /// Whether this is the last step of an `ensure_installed` run.
    ///
    /// A consumer that sees one of these knows the sender is finished, without waiting
    /// for the channel to close — which it may not do promptly, since the backend holds
    /// the sender until it returns.
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Ready | Self::Failed { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installing_is_what_moves_the_state_machine() {
        assert_eq!(
            PairingProgress::Checking {
                message: "looking for brscan5".to_owned()
            }
            .pairing_state(),
            Some(PairingState::Pairing)
        );
        assert_eq!(
            PairingProgress::Installing {
                package: "brscan5".to_owned(),
                percent: None,
            }
            .pairing_state(),
            Some(PairingState::InstallingBackend)
        );
    }

    /// Dependencies present is not pairing done — the listener still has to start.
    #[test]
    fn ready_implies_no_transition() {
        assert_eq!(PairingProgress::Ready.pairing_state(), None);
        assert!(PairingProgress::Ready.is_terminal());
    }

    #[test]
    fn a_failure_carries_its_message_into_the_state() {
        let progress = PairingProgress::Failed {
            message: "installing brscan5 failed: 404".to_owned(),
        };

        let state = progress.pairing_state().unwrap();
        assert!(state.is_failed());
        assert_eq!(state.pairing_error(), "installing brscan5 failed: 404");
        assert!(progress.is_terminal());
    }

    #[test]
    fn the_non_terminal_steps_say_so() {
        assert!(
            !PairingProgress::Checking {
                message: String::new()
            }
            .is_terminal()
        );
        assert!(
            !PairingProgress::Installing {
                package: "brscan-skey".to_owned(),
                percent: Some(40),
            }
            .is_terminal()
        );
    }

    #[test]
    fn a_button_trigger_carries_the_key_and_nothing_about_the_profile() {
        let id = ScannerId::from_backend("mock", "usb:001:002").unwrap();
        let event = ScanTrigger::button(id.clone(), "job-1", 2);

        assert_eq!(event.scanner_id, id);
        assert_eq!(event.id, "job-1");
        assert_eq!(event.kind, TriggerKind::Button { index: 2 });
        assert!(event.timestamp.elapsed().is_ok());
    }

    #[test]
    fn a_push_trigger_carries_the_profile() {
        let id = ScannerId::from_backend("mobile", "phone_a1b2c3").unwrap();
        let event = ScanTrigger::push(id.clone(), "upload-1", ProfileKind::Image);

        assert_eq!(event.scanner_id, id);
        assert_eq!(event.id, "upload-1");
        assert_eq!(
            event.kind,
            TriggerKind::Push {
                profile: ProfileKind::Image
            }
        );
        assert!(event.timestamp.elapsed().is_ok());
    }
}
