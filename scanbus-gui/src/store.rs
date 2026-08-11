use std::collections::{BTreeMap, HashMap};

use scanbus_client::{DecodeError, ScannerState};
use scanbus_core::{PairingState, ProfileKind, ScannerId, path};
use zbus::zvariant::{OwnedValue, Value as ZValue};

pub type Dict = HashMap<String, OwnedValue>;
pub type ManagedSnapshot = HashMap<String, HashMap<String, Dict>>;

const SCANNER_INTERFACE: &str = "org.scanbus.Scanner1";
const BUTTON_INTERFACE: &str = "org.scanbus.Button1";
const JOB_INTERFACE: &str = "org.scanbus.Job1";
const PROFILE_INTERFACE: &str = "org.scanbus.Profile1";

#[derive(Debug, Clone, PartialEq)]
pub enum StoreEvent {
    ServicePresent(bool),
    Replace(ManagedSnapshot),
    InterfacesAdded {
        path: String,
        interfaces: HashMap<String, Dict>,
    },
    InterfacesRemoved {
        path: String,
        interfaces: Vec<String>,
    },
    PropertiesChanged {
        path: String,
        interface: String,
        changed: Dict,
        invalidated: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ServiceState {
    #[default]
    Unknown,
    Running,
    Absent,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DiscoveryState {
    #[default]
    Idle,
    Starting,
    Active,
    Stopping,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Store {
    pub service: ServiceState,
    pub scanners: BTreeMap<ScannerId, ScannerEntry>,
    pub profiles: BTreeMap<ProfileKind, ProfileEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScannerEntry {
    pub path: String,
    pub state: ScannerState,
    pub buttons: BTreeMap<u32, ButtonEntry>,
    pub jobs: BTreeMap<u64, JobEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ButtonEntry {
    pub index: u32,
    pub device_label: String,
    pub label_configurable: bool,
    pub label: String,
    pub profile: Option<ProfileKind>,
    pub profile_options: Dict,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobEntry {
    pub scanner_path: String,
    pub button: i32,
    pub profile: Option<ProfileKind>,
    pub state: String,
    pub page_count: u32,
    pub result: Dict,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProfileEntry {
    pub kind: ProfileKind,
    pub options: Dict,
}

impl Store {
    pub fn apply(&mut self, event: StoreEvent) -> Result<(), DecodeError> {
        match event {
            StoreEvent::ServicePresent(true) => self.service = ServiceState::Running,
            StoreEvent::ServicePresent(false) => {
                self.service = ServiceState::Absent;
                self.scanners.clear();
                self.profiles.clear();
            }
            StoreEvent::Replace(snapshot) => self.replace(snapshot)?,
            StoreEvent::InterfacesAdded { path, interfaces } => {
                self.apply_interfaces_added(&path, &interfaces)?
            }
            StoreEvent::InterfacesRemoved { path, interfaces } => {
                self.apply_interfaces_removed(&path, &interfaces)
            }
            StoreEvent::PropertiesChanged {
                path,
                interface,
                changed,
                invalidated,
            } => self.apply_properties_changed(&path, &interface, &changed, &invalidated)?,
        }

        Ok(())
    }

    fn replace(&mut self, snapshot: ManagedSnapshot) -> Result<(), DecodeError> {
        self.scanners.clear();
        self.profiles.clear();

        for (path, interfaces) in &snapshot {
            if interfaces.contains_key(SCANNER_INTERFACE)
                || interfaces.contains_key(PROFILE_INTERFACE)
            {
                self.apply_interfaces_added(path, interfaces)?;
            }
        }

        for (path, interfaces) in snapshot {
            if !(interfaces.contains_key(BUTTON_INTERFACE)
                || interfaces.contains_key(JOB_INTERFACE))
            {
                continue;
            }

            self.apply_interfaces_added(&path, &interfaces)?;
        }

        Ok(())
    }

    fn apply_interfaces_added(
        &mut self,
        object_path: &str,
        interfaces: &HashMap<String, Dict>,
    ) -> Result<(), DecodeError> {
        if let Some(properties) = interfaces.get(SCANNER_INTERFACE) {
            let state = ScannerState::from_properties(properties)?;
            self.scanners.insert(
                state.id.clone(),
                ScannerEntry {
                    path: object_path.to_owned(),
                    state,
                    buttons: BTreeMap::new(),
                    jobs: BTreeMap::new(),
                },
            );
        }

        if let Some(properties) = interfaces.get(BUTTON_INTERFACE)
            && let Some((scanner, index)) = path::button_index(object_path)
            && let Some(entry) = self.scanners.get_mut(&scanner)
        {
            entry
                .buttons
                .insert(index, ButtonEntry::from_properties(properties)?);
        }

        if let Some(properties) = interfaces.get(JOB_INTERFACE)
            && let Some((scanner, job_id)) = path::job_id(object_path)
            && let Some(entry) = self.scanners.get_mut(&scanner)
        {
            entry
                .jobs
                .insert(job_id, JobEntry::from_properties(properties)?);
        }

        if let Some(properties) = interfaces.get(PROFILE_INTERFACE)
            && let Some(kind) = path::profile_kind(object_path)
        {
            self.profiles.insert(
                kind,
                ProfileEntry {
                    kind,
                    options: dict(properties, "Options")?,
                },
            );
        }

        Ok(())
    }

    fn apply_interfaces_removed(&mut self, object_path: &str, interfaces: &[String]) {
        if interfaces.iter().any(|name| name == SCANNER_INTERFACE)
            && let Some(id) = path::scanner_id(object_path)
        {
            self.scanners.remove(&id);
        }

        if interfaces.iter().any(|name| name == BUTTON_INTERFACE)
            && let Some((scanner, index)) = path::button_index(object_path)
            && let Some(entry) = self.scanners.get_mut(&scanner)
        {
            entry.buttons.remove(&index);
        }

        if interfaces.iter().any(|name| name == JOB_INTERFACE)
            && let Some((scanner, job_id)) = path::job_id(object_path)
            && let Some(entry) = self.scanners.get_mut(&scanner)
        {
            entry.jobs.remove(&job_id);
        }

        if interfaces.iter().any(|name| name == PROFILE_INTERFACE)
            && let Some(kind) = path::profile_kind(object_path)
        {
            self.profiles.remove(&kind);
        }
    }

    fn apply_properties_changed(
        &mut self,
        object_path: &str,
        interface: &str,
        changed: &Dict,
        invalidated: &[String],
    ) -> Result<(), DecodeError> {
        if !invalidated.is_empty() {
            return Err(DecodeError::Missing(invalidated[0].clone()));
        }

        match interface {
            SCANNER_INTERFACE => {
                if let Some(id) = path::scanner_id(object_path)
                    && let Some(entry) = self.scanners.get_mut(&id)
                {
                    entry.state.apply(&borrowed(changed))?;
                }
            }
            BUTTON_INTERFACE => {
                if let Some((scanner, index)) = path::button_index(object_path)
                    && let Some(entry) = self.scanners.get_mut(&scanner)
                    && let Some(button) = entry.buttons.get_mut(&index)
                {
                    button.apply(changed)?;
                }
            }
            JOB_INTERFACE => {
                if let Some((scanner, job_id)) = path::job_id(object_path)
                    && let Some(entry) = self.scanners.get_mut(&scanner)
                    && let Some(job) = entry.jobs.get_mut(&job_id)
                {
                    job.apply(changed)?;
                }
            }
            PROFILE_INTERFACE => {
                if let Some(kind) = path::profile_kind(object_path)
                    && let Some(profile) = self.profiles.get_mut(&kind)
                    && let Some(options) = changed.get("Options")
                {
                    profile.options = dict_value(options, "Options")?;
                }
            }
            _ => {}
        }

        Ok(())
    }
}

impl ButtonEntry {
    fn from_properties(properties: &Dict) -> Result<Self, DecodeError> {
        Ok(Self {
            index: unsigned(properties, "Index")?,
            device_label: string(properties, "DeviceLabel")?,
            label_configurable: flag(properties, "LabelConfigurable")?,
            label: string(properties, "Label")?,
            profile: ProfileKind::parse_optional(&string(properties, "Profile")?)?,
            profile_options: dict(properties, "ProfileOptions")?,
        })
    }

    fn apply(&mut self, changed: &Dict) -> Result<(), DecodeError> {
        for (key, value) in changed {
            match key.as_str() {
                "Index" => self.index = unsigned_value(value, "Index")?,
                "DeviceLabel" => self.device_label = string_value(value, "DeviceLabel")?,
                "LabelConfigurable" => {
                    self.label_configurable = flag_value(value, "LabelConfigurable")?
                }
                "Label" => self.label = string_value(value, "Label")?,
                "Profile" => {
                    self.profile = ProfileKind::parse_optional(&string_value(value, "Profile")?)?
                }
                "ProfileOptions" => self.profile_options = dict_value(value, "ProfileOptions")?,
                _ => {}
            }
        }

        Ok(())
    }
}

impl JobEntry {
    fn from_properties(properties: &Dict) -> Result<Self, DecodeError> {
        Ok(Self {
            scanner_path: object_path(properties, "Scanner")?,
            button: integer(properties, "Button")?,
            profile: ProfileKind::parse_optional(&string(properties, "Profile")?)?,
            state: string(properties, "State")?,
            page_count: unsigned(properties, "PageCount")?,
            result: dict(properties, "Result")?,
            error: string(properties, "Error")?,
        })
    }

    fn apply(&mut self, changed: &Dict) -> Result<(), DecodeError> {
        for (key, value) in changed {
            match key.as_str() {
                "Scanner" => self.scanner_path = object_path_value(value, "Scanner")?,
                "Button" => self.button = integer_value(value, "Button")?,
                "Profile" => {
                    self.profile = ProfileKind::parse_optional(&string_value(value, "Profile")?)?
                }
                "State" => self.state = string_value(value, "State")?,
                "PageCount" => self.page_count = unsigned_value(value, "PageCount")?,
                "Result" => self.result = dict_value(value, "Result")?,
                "Error" => self.error = string_value(value, "Error")?,
                _ => {}
            }
        }

        Ok(())
    }
}

fn borrowed(properties: &Dict) -> HashMap<&str, ZValue<'_>> {
    properties
        .iter()
        .filter_map(|(key, value)| {
            let cloned = value.try_clone().ok()?;
            Some((key.as_str(), cloned.into()))
        })
        .collect()
}

fn get<'a>(properties: &'a Dict, key: &str) -> Result<&'a OwnedValue, DecodeError> {
    properties
        .get(key)
        .ok_or_else(|| DecodeError::Missing(key.to_owned()))
}

fn string(properties: &Dict, key: &str) -> Result<String, DecodeError> {
    string_value(get(properties, key)?, key)
}

fn flag(properties: &Dict, key: &str) -> Result<bool, DecodeError> {
    flag_value(get(properties, key)?, key)
}

fn unsigned(properties: &Dict, key: &str) -> Result<u32, DecodeError> {
    unsigned_value(get(properties, key)?, key)
}

fn integer(properties: &Dict, key: &str) -> Result<i32, DecodeError> {
    integer_value(get(properties, key)?, key)
}

fn object_path(properties: &Dict, key: &str) -> Result<String, DecodeError> {
    object_path_value(get(properties, key)?, key)
}

fn dict(properties: &Dict, key: &str) -> Result<Dict, DecodeError> {
    dict_value(get(properties, key)?, key)
}

fn string_value(value: &OwnedValue, key: &str) -> Result<String, DecodeError> {
    match Into::<ZValue<'_>>::into(value.try_clone().map_err(|_| wrong_type(key, "s", value))?) {
        ZValue::Str(text) => Ok(text.as_str().to_owned()),
        _ => Err(wrong_type(key, "s", value)),
    }
}

fn flag_value(value: &OwnedValue, key: &str) -> Result<bool, DecodeError> {
    match Into::<ZValue<'_>>::into(value.try_clone().map_err(|_| wrong_type(key, "b", value))?) {
        ZValue::Bool(flag) => Ok(flag),
        _ => Err(wrong_type(key, "b", value)),
    }
}

fn unsigned_value(value: &OwnedValue, key: &str) -> Result<u32, DecodeError> {
    let cloned = value.try_clone().map_err(|_| wrong_type(key, "u", value))?;
    scanbus_client::convert::from_variant(&Into::<ZValue<'_>>::into(cloned))
        .map_err(|_| wrong_type(key, "u", value))?
        .as_u64()
        .and_then(|number| u32::try_from(number).ok())
        .ok_or_else(|| wrong_type(key, "u", value))
}

fn integer_value(value: &OwnedValue, key: &str) -> Result<i32, DecodeError> {
    let cloned = value.try_clone().map_err(|_| wrong_type(key, "i", value))?;
    scanbus_client::convert::from_variant(&Into::<ZValue<'_>>::into(cloned))
        .map_err(|_| wrong_type(key, "i", value))?
        .as_i64()
        .and_then(|number| i32::try_from(number).ok())
        .ok_or_else(|| wrong_type(key, "i", value))
}

fn object_path_value(value: &OwnedValue, key: &str) -> Result<String, DecodeError> {
    match Into::<ZValue<'_>>::into(value.try_clone().map_err(|_| wrong_type(key, "o", value))?) {
        ZValue::ObjectPath(path) => Ok(path.as_str().to_owned()),
        _ => Err(wrong_type(key, "o", value)),
    }
}

fn dict_value(value: &OwnedValue, key: &str) -> Result<Dict, DecodeError> {
    value
        .try_clone()
        .map_err(|_| wrong_type(key, "a{sv}", value))?
        .try_into()
        .map_err(|_| wrong_type(key, "a{sv}", value))
}

fn wrong_type(key: &str, expected: &'static str, got: &OwnedValue) -> DecodeError {
    DecodeError::Type {
        key: key.to_owned(),
        expected,
        signature: got.value_signature().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scanbus_core::{Capabilities, Status};

    fn owned(value: ZValue<'_>) -> OwnedValue {
        OwnedValue::try_from(value).unwrap()
    }

    fn scanner_id() -> ScannerId {
        ScannerId::from_backend("mock", "usb:001:002").unwrap()
    }

    fn scanner_path() -> String {
        path::scanner(&scanner_id())
    }

    fn scanner_state() -> ScannerState {
        ScannerState {
            id: scanner_id(),
            name: "Brother".to_owned(),
            backend: "proprietary:brother".to_owned(),
            address: "usb:001:002".to_owned(),
            capabilities: Capabilities::default(),
            supported_profiles: vec![ProfileKind::Document, ProfileKind::Image],
            paired: false,
            connected: false,
            status: Status::Offline,
            default_profile: Some(ProfileKind::Document),
            pairing: PairingState::None,
        }
    }

    fn scanner_dict() -> Dict {
        HashMap::from([
            ("Id".to_owned(), owned(ZValue::from(scanner_id().as_str()))),
            ("Name".to_owned(), owned(ZValue::from("Brother"))),
            (
                "Backend".to_owned(),
                owned(ZValue::from("proprietary:brother")),
            ),
            ("Address".to_owned(), owned(ZValue::from("usb:001:002"))),
            (
                "Capabilities".to_owned(),
                owned(ZValue::from(HashMap::<String, OwnedValue>::new())),
            ),
            (
                "SupportedProfiles".to_owned(),
                owned(ZValue::from(vec![
                    "document".to_owned(),
                    "image".to_owned(),
                ])),
            ),
            ("Paired".to_owned(), owned(ZValue::from(false))),
            ("Connected".to_owned(), owned(ZValue::from(false))),
            ("Status".to_owned(), owned(ZValue::from("offline"))),
            ("DefaultProfile".to_owned(), owned(ZValue::from("document"))),
            ("PairingState".to_owned(), owned(ZValue::from("none"))),
            ("PairingError".to_owned(), owned(ZValue::from(""))),
            (
                "PairingInfo".to_owned(),
                owned(ZValue::from(HashMap::<String, OwnedValue>::new())),
            ),
        ])
    }

    fn button_dict(index: u32) -> Dict {
        HashMap::from([
            ("Index".to_owned(), owned(ZValue::from(index))),
            ("DeviceLabel".to_owned(), owned(ZValue::from("Scan"))),
            ("LabelConfigurable".to_owned(), owned(ZValue::from(false))),
            ("Label".to_owned(), owned(ZValue::from("Scan"))),
            ("Profile".to_owned(), owned(ZValue::from("document"))),
            (
                "ProfileOptions".to_owned(),
                owned(ZValue::from(HashMap::<String, OwnedValue>::new())),
            ),
        ])
    }

    #[test]
    fn snapshot_populates_scanners_and_buttons() {
        let mut store = Store::default();
        let button_path = path::button(&scanner_id(), 0);

        store
            .apply(StoreEvent::Replace(HashMap::from([
                (
                    scanner_path(),
                    HashMap::from([(SCANNER_INTERFACE.to_owned(), scanner_dict())]),
                ),
                (
                    button_path,
                    HashMap::from([(BUTTON_INTERFACE.to_owned(), button_dict(0))]),
                ),
            ])))
            .unwrap();

        let scanner = store.scanners.get(&scanner_id()).unwrap();
        assert_eq!(scanner.state, scanner_state());
        assert_eq!(scanner.buttons.len(), 1);
        assert_eq!(scanner.buttons[&0].profile, Some(ProfileKind::Document));
    }

    #[test]
    fn properties_changed_updates_only_the_named_scanner_fields() {
        let mut store = Store {
            service: ServiceState::Running,
            scanners: BTreeMap::from([(
                scanner_id(),
                ScannerEntry {
                    path: scanner_path(),
                    state: scanner_state(),
                    buttons: BTreeMap::new(),
                    jobs: BTreeMap::new(),
                },
            )]),
            profiles: BTreeMap::new(),
        };

        store
            .apply(StoreEvent::PropertiesChanged {
                path: scanner_path(),
                interface: SCANNER_INTERFACE.to_owned(),
                changed: HashMap::from([("Status".to_owned(), owned(ZValue::from("online")))]),
                invalidated: Vec::new(),
            })
            .unwrap();

        let scanner = &store.scanners[&scanner_id()].state;
        assert_eq!(scanner.status, Status::Online);
        assert_eq!(scanner.name, "Brother");
        assert_eq!(scanner.pairing, PairingState::None);
    }

    #[test]
    fn marking_the_service_absent_clears_the_store() {
        let mut store = Store {
            service: ServiceState::Running,
            scanners: BTreeMap::from([(
                scanner_id(),
                ScannerEntry {
                    path: scanner_path(),
                    state: scanner_state(),
                    buttons: BTreeMap::new(),
                    jobs: BTreeMap::new(),
                },
            )]),
            profiles: BTreeMap::new(),
        };

        store.apply(StoreEvent::ServicePresent(false)).unwrap();

        assert_eq!(store.service, ServiceState::Absent);
        assert!(store.scanners.is_empty());
        assert!(store.profiles.is_empty());
    }
}
