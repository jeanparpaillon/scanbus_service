//! [`Capabilities`]: what the `a{sv}` of §3 looks like once it is typed.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::Value;

/// What a scanner can do — the `Capabilities` property (§3).
///
/// On the wire this is `{"resolutions":[…],"color_modes":[…],"sources":[…],
/// "duplex":true,"buttons":{"count":4,"label_configurable":false}}`. Every one of those
/// keys is consumed by code that branches on it — `buttons.count` decides how many
/// `Button1` objects are exported (2.5), `sources` decides whether ADF multi-page is
/// possible at all (3.3) — so a map lookup with a typo would mean a scanner that
/// silently exposes zero buttons. Fields make that a compile error.
///
/// [`Capabilities::extra`] keeps the map's openness where it is actually needed: keys a
/// backend reports that this version has no field for survive a round-trip instead of
/// being dropped.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    /// Supported optical resolutions, in DPI.
    pub resolutions: Vec<u32>,
    /// Supported colour modes.
    pub color_modes: Vec<ColorMode>,
    /// Paper paths the device offers.
    pub sources: Vec<Source>,
    /// Whether the ADF can scan both sides.
    pub duplex: bool,
    /// The device's physical menu.
    pub buttons: ButtonsCapability,
    /// Keys this version of the daemon does not know about, preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Capabilities {
    /// Whether the device has an automatic document feeder.
    ///
    /// The multi-page path (3.3) is only reachable when this is true; a flatbed-only
    /// scanner produces exactly one page per job.
    pub fn supports_adf(&self) -> bool {
        self.supports_source(Source::Adf)
    }

    /// Whether `source` is one of the paper paths the device offers.
    pub fn supports_source(&self, source: Source) -> bool {
        self.sources.contains(&source)
    }

    /// How many `Button1` objects this scanner gets (2.5).
    pub const fn button_count(&self) -> u32 {
        self.buttons.count
    }
}

/// A colour mode, as `color_modes` spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorMode {
    /// Full colour.
    Color,
    /// Greyscale.
    Gray,
    /// Bitonal, one bit per pixel.
    Bw,
}

/// A paper path, as `sources` spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The glass.
    Flatbed,
    /// The automatic document feeder.
    Adf,
}

/// The `buttons` sub-map: the device's physical menu, before anything is assigned to it.
///
/// `count` is the whole reason `Capabilities` is not passed around as a map — it is read
/// once at pairing time and decides how many `Button1` objects exist for the scanner's
/// lifetime.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct ButtonsCapability {
    /// Number of entries in the physical menu — dedicated keys, or touchscreen entries.
    pub count: u32,
    /// Whether the host can push a custom label to the device.
    ///
    /// `false` on classic Brother devices with fixed keys, where only the key→profile
    /// mapping is configurable (§9).
    pub label_configurable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brother_mfc() -> Capabilities {
        Capabilities {
            resolutions: vec![100, 200, 300, 600],
            color_modes: vec![ColorMode::Color, ColorMode::Gray, ColorMode::Bw],
            sources: vec![Source::Flatbed, Source::Adf],
            duplex: true,
            buttons: ButtonsCapability {
                count: 4,
                label_configurable: false,
            },
            extra: BTreeMap::new(),
        }
    }

    /// The example from §3, spelled exactly as the property carries it.
    #[test]
    fn deserialises_the_documented_example() {
        let json = r#"{
            "resolutions": [100, 200, 300, 600],
            "color_modes": ["color", "gray", "bw"],
            "sources": ["flatbed", "adf"],
            "duplex": true,
            "buttons": {"count": 4, "label_configurable": false}
        }"#;

        assert_eq!(
            serde_json::from_str::<Capabilities>(json).unwrap(),
            brother_mfc()
        );
    }

    #[test]
    fn round_trips() {
        let caps = brother_mfc();
        let json = serde_json::to_string(&caps).unwrap();
        assert_eq!(serde_json::from_str::<Capabilities>(&json).unwrap(), caps);
    }

    /// A backend that reports more than we model must not lose it on the way through.
    #[test]
    fn an_unknown_key_survives_a_round_trip() {
        let json = r#"{
            "resolutions": [300],
            "color_modes": ["gray"],
            "sources": ["flatbed"],
            "duplex": false,
            "buttons": {"count": 0, "label_configurable": false},
            "max_scan_area_mm": [216, 356],
            "vendor": {"brscan": "brscan5"}
        }"#;

        let caps: Capabilities = serde_json::from_str(json).unwrap();
        assert_eq!(
            caps.extra.get("max_scan_area_mm"),
            Some(&Value::Array(vec![Value::U64(216), Value::U64(356)]))
        );
        assert_eq!(
            caps.extra.get("vendor"),
            Some(&Value::Dict(BTreeMap::from([(
                "brscan".to_owned(),
                Value::Str("brscan5".to_owned())
            )])))
        );

        let round_tripped: Capabilities =
            serde_json::from_str(&serde_json::to_string(&caps).unwrap()).unwrap();
        assert_eq!(round_tripped, caps);
    }

    /// A backend that reports less than we model must not fail to parse either.
    #[test]
    fn missing_keys_default_rather_than_failing() {
        let caps: Capabilities = serde_json::from_str("{}").unwrap();
        assert_eq!(caps, Capabilities::default());
        assert_eq!(caps.button_count(), 0);
        assert!(!caps.supports_adf());
    }

    #[test]
    fn adf_is_read_from_sources() {
        assert!(brother_mfc().supports_adf());
        assert!(brother_mfc().supports_source(Source::Flatbed));

        let flatbed_only = Capabilities {
            sources: vec![Source::Flatbed],
            ..brother_mfc()
        };
        assert!(!flatbed_only.supports_adf());
    }

    #[test]
    fn button_count_drives_the_button_objects() {
        assert_eq!(brother_mfc().button_count(), 4);
    }
}
