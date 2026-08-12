use std::collections::{BTreeMap, HashMap};

use scanbus_client::{DecodeError, OptionsSchema, ScannerState};
use scanbus_core::{ProfileKind, ScannerId, path};
use zbus::zvariant::{OwnedValue, Value as ZValue};

pub type Dict = HashMap<String, OwnedValue>;
pub type ManagedSnapshot = HashMap<String, HashMap<String, Dict>>;

const SCANNER_INTERFACE: &str = "org.scanbus.Scanner1";
const BUTTON_INTERFACE: &str = "org.scanbus.Button1";
const JOB_INTERFACE: &str = "org.scanbus.Job1";
const PROFILE_INTERFACE: &str = "org.scanbus.Profile1";

#[derive(Debug, Clone, PartialEq)]
pub enum StoreEvent {
    ServiceState(ServiceState),
    ServiceDetails(ServiceDetails),
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
    Activatable,
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
pub struct ServiceDetails {
    pub version: Option<String>,
    pub backends: Vec<String>,
    pub profile_types: Vec<String>,
    pub output_folders: BTreeMap<ProfileKind, String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Store {
    pub service: ServiceState,
    pub details: ServiceDetails,
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
    pub schema: OptionsSchema,
}

impl Store {
    pub fn apply(&mut self, event: StoreEvent) -> Result<(), DecodeError> {
        match event {
            StoreEvent::ServiceState(state) => {
                self.service = state;
                if state != ServiceState::Running {
                    self.details = ServiceDetails::default();
                    self.scanners.clear();
                    self.profiles.clear();
                }
            }
            StoreEvent::ServiceDetails(details) => {
                self.details = details;
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
                    schema: schema(properties)?,
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
                {
                    if let Some(value) = changed.get("Options") {
                        profile.options = dict_value(value, "Options")?;
                    }
                    if let Some(value) = changed.get("OptionsSchema") {
                        profile.schema = schema_value(value, "OptionsSchema")?;
                    }
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

/// `Profile1.OptionsSchema`, or an empty schema when the profile does not publish one.
///
/// Absence is the one thing tolerated here, and only because of what the alternative
/// costs: a daemon older than API §6's property would fail this decode, and a failed
/// decode drops the whole `GetManagedObjects` snapshot — every scanner with it. An empty
/// schema costs the Profiles page instead of the window. A property that *is* published
/// and does not decode stays an error, the same as a `Options` of the wrong type: that is
/// a daemon bug, and swallowing it would hide it.
fn schema(properties: &Dict) -> Result<OptionsSchema, DecodeError> {
    match properties.get("OptionsSchema") {
        Some(value) => schema_value(value, "OptionsSchema"),
        None => Ok(OptionsSchema::default()),
    }
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

fn schema_value(value: &OwnedValue, key: &str) -> Result<OptionsSchema, DecodeError> {
    let dict = dict_value(value, key)?;
    OptionsSchema::decode(&dict)
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
    use scanbus_core::{Capabilities, PairingState, Status};

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
            details: ServiceDetails::default(),
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

    fn schema_entry(fields: &[(&str, OwnedValue)]) -> OwnedValue {
        owned(ZValue::from(
            fields
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.try_clone().unwrap()))
                .collect::<HashMap<String, OwnedValue>>(),
        ))
    }

    fn profile_dict(schema: Option<OwnedValue>) -> Dict {
        let mut properties = HashMap::from([
            ("Name".to_owned(), owned(ZValue::from("image"))),
            (
                "Options".to_owned(),
                owned(ZValue::from(HashMap::from([(
                    "format".to_owned(),
                    owned(ZValue::from("png")),
                )]))),
            ),
        ]);
        if let Some(schema) = schema {
            properties.insert("OptionsSchema".to_owned(), schema);
        }
        properties
    }

    fn quality_schema(max: u64) -> OwnedValue {
        owned(ZValue::from(HashMap::from([(
            "quality".to_owned(),
            schema_entry(&[
                ("type", owned(ZValue::from("integer"))),
                ("default", owned(ZValue::from(90u64))),
                ("min", owned(ZValue::from(1u64))),
                ("max", owned(ZValue::from(max))),
            ]),
        )])))
    }

    #[test]
    fn a_profile_arrives_with_its_schema_decoded() {
        let mut store = Store::default();

        store
            .apply(StoreEvent::Replace(HashMap::from([(
                path::profile(ProfileKind::Image),
                HashMap::from([(
                    PROFILE_INTERFACE.to_owned(),
                    profile_dict(Some(quality_schema(100))),
                )]),
            )])))
            .unwrap();

        let profile = &store.profiles[&ProfileKind::Image];
        assert_eq!(
            profile
                .schema
                .get("quality")
                .map(|entry| entry.value.clone()),
            Some(scanbus_client::OptionType::Integer {
                min: Some(1),
                max: Some(100),
            })
        );
    }

    /// API §6: the effective default is computed, so the schema is read-only but *not*
    /// constant — a client that cached it at startup would go stale. Both properties
    /// therefore update independently.
    #[test]
    fn the_schema_changes_without_the_options_changing() {
        let mut store = Store::default();
        store
            .apply(StoreEvent::Replace(HashMap::from([(
                path::profile(ProfileKind::Image),
                HashMap::from([(
                    PROFILE_INTERFACE.to_owned(),
                    profile_dict(Some(quality_schema(100))),
                )]),
            )])))
            .unwrap();

        store
            .apply(StoreEvent::PropertiesChanged {
                path: path::profile(ProfileKind::Image),
                interface: PROFILE_INTERFACE.to_owned(),
                changed: HashMap::from([("OptionsSchema".to_owned(), quality_schema(95))]),
                invalidated: Vec::new(),
            })
            .unwrap();

        let profile = &store.profiles[&ProfileKind::Image];
        assert_eq!(
            profile
                .schema
                .get("quality")
                .and_then(|entry| match entry.value {
                    scanbus_client::OptionType::Integer { max, .. } => max,
                    _ => None,
                }),
            Some(95)
        );
        assert!(
            profile.options.contains_key("format"),
            "a schema change must not clear the stored options"
        );
    }

    /// A daemon older than the property costs the Profiles page, not the whole snapshot:
    /// a failed decode here would drop the scanners that arrived in the same message.
    #[test]
    fn a_profile_without_a_schema_still_reaches_the_store() {
        let mut store = Store::default();

        store
            .apply(StoreEvent::Replace(HashMap::from([
                (
                    path::profile(ProfileKind::Image),
                    HashMap::from([(PROFILE_INTERFACE.to_owned(), profile_dict(None))]),
                ),
                (
                    scanner_path(),
                    HashMap::from([(SCANNER_INTERFACE.to_owned(), scanner_dict())]),
                ),
            ])))
            .unwrap();

        assert!(store.profiles[&ProfileKind::Image].schema.is_empty());
        assert!(store.scanners.contains_key(&scanner_id()));
    }

    #[test]
    fn marking_the_service_absent_clears_the_store() {
        let mut store = Store {
            service: ServiceState::Running,
            details: ServiceDetails::default(),
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
            .apply(StoreEvent::ServiceState(ServiceState::Absent))
            .unwrap();

        assert_eq!(store.service, ServiceState::Absent);
        assert!(store.scanners.is_empty());
        assert!(store.profiles.is_empty());
    }
}
