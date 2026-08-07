//! [`Status`] and [`PairingState`]: the two closed string enumerations of `Scanner1`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::ParseError;

/// Reachability of a scanner — the `Status` property (§3).
///
/// Independent of whether it is paired: a paired scanner that is switched off is
/// [`Status::Offline`] and stays paired (§9).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Not reachable — powered off, or off the network.
    #[default]
    Offline,
    /// Reachable and idle.
    Online,
    /// Reachable, but busy with a scan.
    Busy,
    /// Reachable and reporting a fault.
    Error,
}

impl Status {
    /// Every variant, for exhaustive tests and for enumerating the API.
    pub const ALL: [Status; 4] = [Self::Offline, Self::Online, Self::Busy, Self::Error];

    /// The exact string the `Status` property carries.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::Online => "online",
            Self::Busy => "busy",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Status {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == s)
            .ok_or_else(|| ParseError::new("Status", s))
    }
}

/// Where a scanner is in the pairing process — the `PairingState` property (§3, §6).
///
/// D-Bus splits this over two properties: `PairingState` as a string and `PairingError`
/// as the detail, "empty otherwise". That pair has an invariant no D-Bus type can
/// express — a non-empty `PairingError` while the state is not `failed` is nonsense,
/// and the way it happens in practice is forgetting to clear the field on a retry. Here
/// the message is *inside* the variant, so the invariant holds by construction:
/// [`PairingState::pairing_error`] has nothing to return unless the state is
/// [`PairingState::Failed`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingState {
    /// Never paired, or unpaired again — also where [`PairingState::Failed`] resets to.
    #[default]
    None,
    /// `Pair()` has been called and the process is running.
    Pairing,
    /// Still pairing, currently installing the backend's system dependencies.
    InstallingBackend,
    /// Paired.
    Done,
    /// The attempt failed; the payload is what `PairingError` reports.
    Failed(String),
}

impl PairingState {
    /// The exact string the `PairingState` property carries.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Pairing => "pairing",
            Self::InstallingBackend => "installing_backend",
            Self::Done => "done",
            Self::Failed(_) => "failed",
        }
    }

    /// The exact string the `PairingError` property carries: empty unless failed.
    pub fn pairing_error(&self) -> &str {
        match self {
            Self::Failed(message) => message,
            _ => "",
        }
    }

    /// Rebuilds a state from the two D-Bus properties.
    ///
    /// `error` is only read for `"failed"`; a client that sends a stale error message
    /// alongside `"done"` gets [`PairingState::Done`] and no trace of it, which is the
    /// same invariant seen from the parsing side.
    ///
    /// # Errors
    ///
    /// Fails if `state` is not one of the five documented strings.
    pub fn from_dbus(state: &str, error: &str) -> Result<Self, ParseError> {
        Ok(match state {
            "none" => Self::None,
            "pairing" => Self::Pairing,
            "installing_backend" => Self::InstallingBackend,
            "done" => Self::Done,
            "failed" => Self::Failed(error.to_owned()),
            _ => return Err(ParseError::new("PairingState", state)),
        })
    }

    /// Whether a pairing attempt is running.
    ///
    /// This is the idempotency test from §9: `Pair()` on a scanner in one of these
    /// states does not restart anything, and `CancelPairing()` is only meaningful here.
    pub const fn is_in_progress(&self) -> bool {
        matches!(self, Self::Pairing | Self::InstallingBackend)
    }

    /// Whether the last attempt failed.
    pub const fn is_failed(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

impl fmt::Display for PairingState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant, `Failed` with a message that must not leak into the state string.
    fn all_pairing_states() -> Vec<PairingState> {
        vec![
            PairingState::None,
            PairingState::Pairing,
            PairingState::InstallingBackend,
            PairingState::Done,
            PairingState::Failed("brscan5 install failed".to_owned()),
        ]
    }

    #[test]
    fn status_matches_the_documented_strings() {
        let expected = ["offline", "online", "busy", "error"];

        for (status, string) in Status::ALL.into_iter().zip(expected) {
            assert_eq!(status.as_str(), string);
            assert_eq!(
                serde_json::to_string(&status).unwrap(),
                format!("\"{string}\"")
            );
            assert_eq!(string.parse::<Status>().unwrap(), status);
            assert_eq!(
                serde_json::from_str::<Status>(&format!("\"{string}\"")).unwrap(),
                status
            );
        }
    }

    #[test]
    fn status_rejects_anything_else() {
        assert!("Online".parse::<Status>().is_err());
        assert!("".parse::<Status>().is_err());
        assert_eq!(
            "sleeping".parse::<Status>().unwrap_err().to_string(),
            "\"sleeping\" is not a valid Status"
        );
    }

    #[test]
    fn pairing_state_matches_the_documented_strings() {
        let expected = ["none", "pairing", "installing_backend", "done", "failed"];

        for (state, string) in all_pairing_states().into_iter().zip(expected) {
            assert_eq!(state.as_str(), string);
            assert_eq!(
                PairingState::from_dbus(string, state.pairing_error()).unwrap(),
                state
            );
        }
    }

    #[test]
    fn pairing_state_round_trips_through_serde_with_its_message() {
        for state in all_pairing_states() {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(serde_json::from_str::<PairingState>(&json).unwrap(), state);
        }
        assert_eq!(
            serde_json::to_string(&PairingState::InstallingBackend).unwrap(),
            "\"installing_backend\""
        );
    }

    /// The property this type exists for.
    #[test]
    fn pairing_error_is_non_empty_exactly_when_failed() {
        for state in all_pairing_states() {
            assert_eq!(
                !state.pairing_error().is_empty(),
                state.is_failed(),
                "{state:?} renders PairingError={:?}",
                state.pairing_error()
            );
        }
    }

    /// A stale error message cannot be smuggled back in alongside a non-failed state.
    #[test]
    fn from_dbus_drops_the_error_unless_the_state_is_failed() {
        for string in ["none", "pairing", "installing_backend", "done"] {
            let state = PairingState::from_dbus(string, "stale message").unwrap();
            assert_eq!(state.pairing_error(), "");
        }

        assert!(PairingState::from_dbus("cancelled", "").is_err());
    }

    #[test]
    fn in_progress_is_where_pair_is_a_no_op_and_cancel_applies() {
        assert!(PairingState::Pairing.is_in_progress());
        assert!(PairingState::InstallingBackend.is_in_progress());
        assert!(!PairingState::None.is_in_progress());
        assert!(!PairingState::Done.is_in_progress());
        assert!(!PairingState::Failed(String::new()).is_in_progress());
    }
}
