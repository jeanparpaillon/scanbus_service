//! [`ButtonInfo`]: one entry of the device's physical menu.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{Capabilities, ProfileKind, Value};

/// One entry of the physical menu — a `Button1` object (§5).
///
/// The interface separates what the firmware imposes from what the host may assign, and
/// the types say so: [`ButtonInfo::device_label`] is a `String` the backend fills in,
/// while [`ButtonInfo::label`] and [`ButtonInfo::profile`] are `Option`s the host owns.
/// `None` is not the same as an empty string here — an unset label means "show what the
/// firmware says", which is why `Label` mirrors `DeviceLabel` when nothing was written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ButtonInfo {
    /// Position in the physical menu, 0-based.
    pub index: u32,
    /// Label the firmware imposes, e.g. `"Scan to E-mail"`. Empty when the device
    /// exposes none of its own.
    pub device_label: String,
    /// Whether the host can push a custom label to the device.
    pub label_configurable: bool,
    /// Label the host has assigned; `None` when it has assigned none.
    pub label: Option<String>,
    /// Profile triggered by this entry; `None` when unassigned.
    ///
    /// Nothing ties this to [`ButtonInfo::device_label`]: a key the firmware calls
    /// "Scan to OCR" may be assigned `document`, and warning the user about that
    /// divergence is a config UI's job (§5).
    pub profile: Option<ProfileKind>,
    /// Options for this profile on this key, e.g. a different output folder per key.
    pub profile_options: BTreeMap<String, Value>,
}

impl ButtonInfo {
    /// A menu entry as the firmware presents it, with nothing assigned yet.
    pub fn new(index: u32, device_label: impl Into<String>, label_configurable: bool) -> Self {
        Self {
            index,
            device_label: device_label.into(),
            label_configurable,
            label: None,
            profile: None,
            profile_options: BTreeMap::new(),
        }
    }

    /// The `Button1` objects to export for a scanner, from its capabilities.
    ///
    /// This is the read of `buttons.count` that decides the object tree (2.5). `count`
    /// alone decides *how many*; the labels are whatever the backend was able to fill
    /// into `buttons.labels` — a device with fixed keys knows them, a generic
    /// touchscreen has none and the entries stay empty.
    pub fn from_capabilities(capabilities: &Capabilities) -> Vec<Self> {
        (0..capabilities.button_count())
            .map(|index| {
                let device_label = capabilities
                    .buttons
                    .labels
                    .get(index as usize)
                    .map_or("", String::as_str);
                Self::new(index, device_label, capabilities.buttons.label_configurable)
            })
            .collect()
    }

    /// The `Label` property: what is actually displayed.
    ///
    /// Falls back to [`ButtonInfo::device_label`], which is what makes a write to
    /// `Label` on a device with fixed keys a no-op rather than a lie.
    pub fn effective_label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.device_label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ButtonsCapability;

    #[test]
    fn button_count_decides_how_many_objects_exist() {
        let capabilities = Capabilities {
            buttons: ButtonsCapability {
                count: 4,
                label_configurable: false,
                labels: Vec::new(),
            },
            ..Capabilities::default()
        };

        let buttons = ButtonInfo::from_capabilities(&capabilities);
        assert_eq!(buttons.len(), 4);
        assert_eq!(
            buttons.iter().map(|b| b.index).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert!(buttons.iter().all(|b| b.profile.is_none()));
        assert!(buttons.iter().all(|b| !b.label_configurable));

        // A scanner without a physical menu gets no Button1 objects, not one empty one.
        assert!(ButtonInfo::from_capabilities(&Capabilities::default()).is_empty());
    }

    #[test]
    fn label_mirrors_the_device_label_until_one_is_assigned() {
        let mut button = ButtonInfo::new(3, "Scan to E-mail", true);
        assert_eq!(button.effective_label(), "Scan to E-mail");

        button.label = Some("Invoices".to_owned());
        assert_eq!(button.effective_label(), "Invoices");
    }

    #[test]
    fn round_trips_with_its_assignment() {
        let button = ButtonInfo {
            profile: Some(ProfileKind::Document),
            profile_options: BTreeMap::from([(
                "output_folder".to_owned(),
                Value::Str("~/Scans/invoices".to_owned()),
            )]),
            ..ButtonInfo::new(2, "Scan to OCR", false)
        };

        let json = serde_json::to_string(&button).unwrap();
        assert_eq!(serde_json::from_str::<ButtonInfo>(&json).unwrap(), button);
    }
}
