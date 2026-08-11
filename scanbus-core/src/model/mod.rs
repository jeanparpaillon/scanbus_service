//! Domain model: scanners, capabilities, buttons, pages.
//!
//! The D-Bus API renders much of this as `a{sv}` (`Capabilities`, `ProfileOptions`,
//! `Job1.Result`) or as bare strings (`Status`, `PairingState`, `Profile`). That is the
//! right choice on the wire — a client has to be able to read a scanner it does not
//! understand — and the wrong one inside the daemon, where every one of those values is
//! branched on: `buttons.count` decides how many `Button1` objects exist, `sources`
//! decides whether ADF multi-page is possible at all.
//!
//! So the model is typed here and rendered at the boundary. The `zvariant` conversions
//! deliberately live in `scanbus-daemon`: this crate must stay buildable and testable
//! without a bus, which `scripts/check-deps.sh` enforces.
//!
//! The one open door is [`Value`], used for the genuinely unstructured corners —
//! per-button profile options, and capability keys a backend knows about and we do not.

mod button;
mod capabilities;
mod id;
mod job;
mod page;
mod profile;
mod state;
mod value;

pub use button::ButtonInfo;
pub use capabilities::{ButtonsCapability, Capabilities, ColorMode, Source};
pub use id::{ScannerId, escape_component, unescape_component};
pub use job::JobState;
pub use page::{PageFormat, RawPage};
pub use profile::ProfileKind;
pub use state::{PairingState, Status};
pub use value::Value;

use serde::{Deserialize, Serialize};

/// Everything a backend knows about a scanner it has discovered.
///
/// This is the backend's half of the `Scanner1` interface. The host's half — `Paired`,
/// `PairingState`, `DefaultProfile`, the per-button assignments — is daemon state and
/// lives in the registry, not here: a backend rediscovering a scanner must not be able
/// to reset the pairing by returning a fresh [`ScannerInfo`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScannerInfo {
    /// Stable identifier; also the `{id}` element of every object path for this scanner.
    pub id: ScannerId,
    /// Human-readable name, as the device reports it.
    pub name: String,
    /// Which subsystem found it: `"sane"`, `"escl"`, `"proprietary:brother"`, …
    ///
    /// This is the D-Bus `Backend` property, free-form on purpose (§3), and not the
    /// backend id that went into [`ScannerId::from_backend`].
    pub backend: String,
    /// Connection URI or device path, e.g. `epson2:net:192.168.1.50`.
    pub address: String,
    /// What the device can do.
    pub capabilities: Capabilities,
    /// Reachability, independent of whether the scanner is paired (§9).
    pub status: Status,
}

impl ScannerInfo {
    /// The `SupportedProfiles` property: profile kinds this daemon will actually run
    /// *and* the device can produce.
    ///
    /// [`ProfileKind::supported`] is the daemon's half — `email` and `ocr` exist in
    /// [`ProfileKind`] and are refused (2.7) — and it is the whole answer for a scanner
    /// this daemon drives itself, where the limit is our pipeline and not the hardware.
    /// The mobile backend is the first with a device that has its own opinion: a phone
    /// sends `capabilities.profiles` when it pairs (9.3), and the two are intersected
    /// rather than unioned, because a profile is only offered when *both* ends can do
    /// it.
    ///
    /// The order is [`ProfileKind::supported`]'s, so the property does not reshuffle
    /// itself according to what a device happened to list first.
    pub fn supported_profiles(&self) -> Vec<ProfileKind> {
        let supported = ProfileKind::supported();
        if self.capabilities.profiles.is_empty() {
            return supported;
        }

        supported
            .into_iter()
            .filter(|kind| self.capabilities.profiles.contains(kind))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ScannerInfo {
        ScannerInfo {
            id: ScannerId::from_backend("sane", "epson2:net:192.168.1.50").unwrap(),
            name: "EPSON XP-7100".to_owned(),
            backend: "sane".to_owned(),
            address: "epson2:net:192.168.1.50".to_owned(),
            capabilities: Capabilities::default(),
            status: Status::Online,
        }
    }

    #[test]
    fn supported_profiles_excludes_the_unimplemented_ones() {
        assert_eq!(
            sample().supported_profiles(),
            vec![ProfileKind::Image, ProfileKind::Document]
        );
    }

    /// A device with an opinion narrows the list; it never widens it.
    #[test]
    fn supported_profiles_intersects_with_what_the_device_advertised() {
        let mut info = sample();
        info.capabilities.profiles = vec![ProfileKind::Image];
        assert_eq!(info.supported_profiles(), vec![ProfileKind::Image]);

        // `ocr` is the device's business and the pipeline's refusal, so it stays out —
        // and the order is the daemon's, not the order the device listed.
        info.capabilities.profiles = vec![ProfileKind::Ocr, ProfileKind::Document];
        assert_eq!(info.supported_profiles(), vec![ProfileKind::Document]);

        // A device that advertises nothing this daemon runs advertises nothing at all,
        // rather than falling back to the full list.
        info.capabilities.profiles = vec![ProfileKind::Ocr];
        assert_eq!(info.supported_profiles(), Vec::<ProfileKind>::new());
    }

    #[test]
    fn scanner_info_round_trips() {
        let info = sample();
        let json = serde_json::to_string(&info).unwrap();
        assert_eq!(serde_json::from_str::<ScannerInfo>(&json).unwrap(), info);
    }
}
