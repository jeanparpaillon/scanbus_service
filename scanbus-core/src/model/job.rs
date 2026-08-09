//! [`JobState`]: the lifecycle of one scan, as `Job1.State` spells it.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::ParseError;

/// Where a scan in flight has got to — the `State` property of
/// [`scanbus-dbus-api.md`] §4.
///
/// The documented sequence is `"receiving"` → `"processing"` → `"done"` / `"error"`, and
/// §9 pins what the middle transition means: the job stays in [`JobState::Receiving`] as
/// long as the backend reports further pages (an ADF batch), and [`JobState::Processing`]
/// marks the end of capture and the start of the post-processing pipeline.
///
/// The failure message lives *inside* [`JobState::Error`] for the same reason it lives
/// inside [`PairingState::Failed`](crate::model::PairingState::Failed): D-Bus splits it
/// over `State` and `Error` as two properties with an invariant no D-Bus type can express
/// — a non-empty `Error` alongside `State="done"` is nonsense, and the way that happens in
/// practice is forgetting to clear the field. Here [`JobState::error`] has nothing to
/// return unless the state is [`JobState::Error`].
///
/// [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    /// Pages are still arriving from the backend.
    #[default]
    Receiving,
    /// Capture is over and the profile pipeline is running.
    Processing,
    /// The pipeline finished; `Result` carries its outcome.
    Done,
    /// The scan failed; the payload is what `Error` reports.
    Error(String),
}

impl JobState {
    /// The exact string the `State` property carries.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Receiving => "receiving",
            Self::Processing => "processing",
            Self::Done => "done",
            Self::Error(_) => "error",
        }
    }

    /// The exact string the `Error` property carries: empty unless the job failed.
    pub fn error(&self) -> &str {
        match self {
            Self::Error(message) => message,
            _ => "",
        }
    }

    /// Whether the job has finished, either way.
    ///
    /// This is what starts the retention window: an object in a terminal state is one no
    /// further transition will move, so it can be removed once clients have had a chance
    /// to read `Result`.
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Error(_))
    }

    /// Rebuilds a state from the two D-Bus properties.
    ///
    /// `error` is only read for `"error"`; a client that sends a stale message alongside
    /// `"done"` gets [`JobState::Done`] and no trace of it, which is the same invariant
    /// seen from the parsing side.
    ///
    /// # Errors
    ///
    /// Fails if `state` is not one of the four documented strings.
    pub fn from_dbus(state: &str, error: &str) -> Result<Self, ParseError> {
        Ok(match state {
            "receiving" => Self::Receiving,
            "processing" => Self::Processing,
            "done" => Self::Done,
            "error" => Self::Error(error.to_owned()),
            _ => return Err(ParseError::new("JobState", state)),
        })
    }
}

impl fmt::Display for JobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for JobState {
    type Err = ParseError;

    /// Parses the `State` property alone, so a failed job comes back with an empty
    /// message. Use [`JobState::from_dbus`] where the `Error` property is available too.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_dbus(s, "")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant, `Error` with a message that must not leak into the state string.
    fn all_states() -> Vec<JobState> {
        vec![
            JobState::Receiving,
            JobState::Processing,
            JobState::Done,
            JobState::Error("the scanner stopped answering mid-transfer".to_owned()),
        ]
    }

    #[test]
    fn matches_the_documented_strings() {
        let expected = ["receiving", "processing", "done", "error"];

        for (state, string) in all_states().into_iter().zip(expected) {
            assert_eq!(state.as_str(), string);
            assert_eq!(JobState::from_dbus(string, state.error()).unwrap(), state);
            assert_eq!(string.parse::<JobState>().unwrap().as_str(), string);
        }

        assert!("cancelled".parse::<JobState>().is_err());
    }

    /// The property this type exists for.
    #[test]
    fn the_error_is_non_empty_exactly_when_the_job_failed() {
        for state in all_states() {
            assert_eq!(
                !state.error().is_empty(),
                matches!(state, JobState::Error(_)),
                "{state:?} renders Error={:?}",
                state.error()
            );
        }
    }

    /// A stale message cannot be smuggled back in alongside a state that did not fail.
    #[test]
    fn from_dbus_drops_the_message_unless_the_state_is_error() {
        for string in ["receiving", "processing", "done"] {
            assert_eq!(
                JobState::from_dbus(string, "stale message")
                    .unwrap()
                    .error(),
                ""
            );
        }
    }

    /// Only the two states the retention window applies to are terminal.
    #[test]
    fn done_and_error_are_the_terminal_states() {
        assert!(!JobState::Receiving.is_terminal());
        assert!(!JobState::Processing.is_terminal());
        assert!(JobState::Done.is_terminal());
        assert!(JobState::Error(String::new()).is_terminal());
    }

    #[test]
    fn round_trips_through_serde_with_its_message() {
        for state in all_states() {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(serde_json::from_str::<JobState>(&json).unwrap(), state);
        }
    }
}
