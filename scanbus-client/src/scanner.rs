//! [`ScannerState`]: every `Scanner1` property of §3, typed.
//!
//! The proxies hand back what the wire carries. This is the layer that turns it into the
//! model — and the layer where a daemon sending `Status="sleeping"` becomes an error with
//! a name in it rather than a string that quietly fails a `==` three call sites later.
//!
//! # Why one struct and not eleven getters
//!
//! Because of `PairingState` and `PairingError`. §3 splits one fact over two properties
//! with an invariant no D-Bus type can express — a non-empty `PairingError` while the
//! state is not `failed` is nonsense — and [`scanbus_core::PairingState`] holds the
//! message *inside* the `Failed` variant precisely so the invariant cannot be broken.
//! Rebuilding it needs both properties at once, and a `PropertiesChanged` is entitled to
//! carry either alone: the daemon emits `PairingError` on its own when a second attempt
//! fails with a different message. A struct that keeps the last full state and applies
//! deltas onto it is the only shape that gets that right.

use std::collections::HashMap;

use scanbus_core::{Capabilities, PairingState, ProfileKind, ScannerId, Status, path};
use zbus::Connection;
use zbus::zvariant::Value as ZValue;

use crate::convert::{self, DecodeError, Dict};
use crate::proxy::SCANNER_INTERFACE;

/// Every property of one `Scanner1` object, in the model's types.
#[derive(Debug, Clone, PartialEq)]
pub struct ScannerState {
    /// `Id` — stable, and the path element that names this object.
    pub id: ScannerId,
    /// `Name` — human-readable, and free to change under a client.
    pub name: String,
    /// `Backend` — which subsystem found it.
    pub backend: String,
    /// `Address` — connection URI or device path.
    pub address: String,
    /// `Capabilities`, decoded; unknown keys survive in [`Capabilities::extra`].
    pub capabilities: Capabilities,
    /// `SupportedProfiles` — what this daemon will run for this scanner.
    pub supported_profiles: Vec<ProfileKind>,
    /// `Paired` — independent of [`ScannerState::status`] (§9).
    pub paired: bool,
    /// `Connected` — whether the host is listening.
    pub connected: bool,
    /// `Status` — reachability.
    pub status: Status,
    /// `DefaultProfile`, with `""` as [`None`].
    pub default_profile: Option<ProfileKind>,
    /// `PairingState` and `PairingError`, as the one value they describe.
    pub pairing: PairingState,
}

impl ScannerState {
    /// Reads this scanner's whole `Scanner1` in one `Properties.GetAll` call.
    ///
    /// The one-shot counterpart to [`crate::watch::ScannerWatch`]: for a command that
    /// only needs a snapshot — `list`, `show`, the initial batch of `discover` — there
    /// is no call to race against, so the subscribe-then-snapshot dance would be pure
    /// overhead.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Bus`] if the object is not there — ordinary for an unpaired
    /// scanner whose discovery session just ended — and [`crate::Error::Decode`] for a
    /// reply this version cannot make sense of.
    pub async fn fetch(connection: &Connection, id: &ScannerId) -> crate::error::Result<Self> {
        let properties = crate::proxy::properties(connection, path::scanner(id)).await?;
        let interface = zbus::names::InterfaceName::try_from(SCANNER_INTERFACE)
            .expect("SCANNER_INTERFACE is a valid interface name");
        let snapshot = properties.get_all(interface).await?;
        Ok(Self::from_properties(&snapshot)?)
    }

    /// The object path this scanner is exported at.
    ///
    /// Named the same as [`crate::select::Scanner::path`] for the same reason: a
    /// consumer that only has a [`ScannerState`] — `discover`'s stream, in particular —
    /// must not need to name [`scanbus_core::ScannerId`] or reach for
    /// `scanbus_core::path` itself just to report which object changed.
    pub fn path(&self) -> String {
        path::scanner(&self.id)
    }

    /// Reads a `Properties.GetAll` reply.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Missing`] for a property §3 requires and the reply does not have —
    /// which is a daemon serving a different interface under our name, not a scanner
    /// with less to say. [`DecodeError::Parse`] for a `Status`, `PairingState` or
    /// profile name outside the sets §3 fixes, and whatever
    /// [`convert::capabilities`] refuses.
    pub fn from_properties(properties: &Dict) -> Result<Self, DecodeError> {
        let pairing_state = string(properties, "PairingState")?;
        let pairing_error = string(properties, "PairingError")?;
        let pairing_info = pairing_info_code(&dict(properties, "PairingInfo")?)?;

        Ok(Self {
            id: ScannerId::new(string(properties, "Id")?)?,
            name: string(properties, "Name")?,
            backend: string(properties, "Backend")?,
            address: string(properties, "Address")?,
            capabilities: convert::capabilities(&dict(properties, "Capabilities")?)?,
            supported_profiles: strings(properties, "SupportedProfiles")?
                .iter()
                .map(|name| name.parse::<ProfileKind>())
                .collect::<Result<_, _>>()?,
            paired: flag(properties, "Paired")?,
            connected: flag(properties, "Connected")?,
            status: string(properties, "Status")?.parse()?,
            default_profile: ProfileKind::parse_optional(&string(properties, "DefaultProfile")?)?,
            pairing: PairingState::from_dbus(&pairing_state, &pairing_error, &pairing_info)?,
        })
    }

    /// Applies the `changed_properties` of a `PropertiesChanged`.
    ///
    /// A property the signal does not mention keeps its value, which is what makes a
    /// signal carrying only `PairingState` yield a whole state rather than a fragment.
    /// Properties this version does not know are ignored: a daemon that grows one must
    /// not stop this client from following the ten it already understands.
    ///
    /// # Errors
    ///
    /// The same as [`ScannerState::from_properties`], for the properties that did change.
    /// `Id` is refused outright — §1 makes it the path element, so an object whose `Id`
    /// moved is a different object and following it silently would attach this state to
    /// the wrong scanner.
    pub fn apply(&mut self, changed: &HashMap<&str, ZValue<'_>>) -> Result<(), DecodeError> {
        // Rebuilt from the three, since any subset may arrive alone.
        let mut state = self.pairing.as_str().to_owned();
        let mut error = self.pairing.pairing_error().to_owned();
        let mut info = self.pairing.pairing_info().to_owned();

        for (property, value) in changed {
            match *property {
                "Id" => {
                    return Err(DecodeError::Type {
                        key: "Id".to_owned(),
                        expected: "unchanged: it is the object's path element",
                        signature: value.value_signature().to_string(),
                    });
                }
                "Name" => self.name = as_string(value, "Name")?,
                "Backend" => self.backend = as_string(value, "Backend")?,
                "Address" => self.address = as_string(value, "Address")?,
                "Capabilities" => {
                    self.capabilities = convert::capabilities(&as_dict(value, "Capabilities")?)?;
                }
                "SupportedProfiles" => {
                    self.supported_profiles = as_strings(value, "SupportedProfiles")?
                        .iter()
                        .map(|name| name.parse::<ProfileKind>())
                        .collect::<Result<_, _>>()?;
                }
                "Paired" => self.paired = as_flag(value, "Paired")?,
                "Connected" => self.connected = as_flag(value, "Connected")?,
                "Status" => self.status = as_string(value, "Status")?.parse()?,
                "DefaultProfile" => {
                    self.default_profile =
                        ProfileKind::parse_optional(&as_string(value, "DefaultProfile")?)?;
                }
                "PairingState" => state = as_string(value, "PairingState")?,
                "PairingError" => error = as_string(value, "PairingError")?,
                "PairingInfo" => info = pairing_info_code(&as_dict(value, "PairingInfo")?)?,
                _ => {}
            }
        }

        self.pairing = PairingState::from_dbus(&state, &error, &info)?;
        Ok(())
    }
}

/// A required property, as a string.
fn string(properties: &Dict, key: &str) -> Result<String, DecodeError> {
    as_string(get(properties, key)?, key)
}

/// A required property, as a boolean.
fn flag(properties: &Dict, key: &str) -> Result<bool, DecodeError> {
    as_flag(get(properties, key)?, key)
}

/// A required property, as an array of strings.
fn strings(properties: &Dict, key: &str) -> Result<Vec<String>, DecodeError> {
    as_strings(get(properties, key)?, key)
}

/// A required property, as an `a{sv}`.
fn dict(properties: &Dict, key: &str) -> Result<Dict, DecodeError> {
    as_dict(get(properties, key)?, key)
}

fn get<'a>(properties: &'a Dict, key: &str) -> Result<&'a ZValue<'a>, DecodeError> {
    properties
        .get(key)
        .map(|value| &**value)
        .ok_or_else(|| DecodeError::Missing(key.to_owned()))
}

fn as_string(value: &ZValue<'_>, key: &str) -> Result<String, DecodeError> {
    match value {
        ZValue::Str(text) => Ok(text.as_str().to_owned()),
        other => Err(wrong_type(key, "s", other)),
    }
}

fn as_flag(value: &ZValue<'_>, key: &str) -> Result<bool, DecodeError> {
    match value {
        ZValue::Bool(flag) => Ok(*flag),
        other => Err(wrong_type(key, "b", other)),
    }
}

fn as_strings(value: &ZValue<'_>, key: &str) -> Result<Vec<String>, DecodeError> {
    let items = match value {
        ZValue::Array(items) => items,
        other => return Err(wrong_type(key, "as", other)),
    };

    items
        .iter()
        .map(|item| match item {
            // `av` as well as `as`: see `convert::string_array` for why both spellings
            // have to be readable.
            ZValue::Value(inner) => as_string(inner, key),
            other => as_string(other, key),
        })
        .collect()
}

fn as_dict(value: &ZValue<'_>, key: &str) -> Result<Dict, DecodeError> {
    match value.try_clone() {
        Ok(cloned) => Dict::try_from(cloned).map_err(|_| wrong_type(key, "a{sv}", value)),
        Err(_) => Err(wrong_type(key, "a{sv}", value)),
    }
}

/// `PairingInfo`'s one documented key, `""` when absent — empty is what every state but
/// `awaiting_confirmation` renders.
fn pairing_info_code(info: &Dict) -> Result<String, DecodeError> {
    match info.get("code") {
        Some(value) => as_string(&**value, "PairingInfo.code"),
        None => Ok(String::new()),
    }
}

fn wrong_type(key: &str, expected: &'static str, got: &ZValue<'_>) -> DecodeError {
    DecodeError::Type {
        key: key.to_owned(),
        expected,
        signature: got.value_signature().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use scanbus_core::{ColorMode, Source};
    use zbus::zvariant::OwnedValue;

    use super::*;

    fn owned(value: ZValue<'_>) -> OwnedValue {
        OwnedValue::try_from(value).unwrap()
    }

    /// A `GetAll` reply from a paired Brother, spelled as the daemon sends it.
    fn brother() -> Dict {
        let buttons: HashMap<String, OwnedValue> = HashMap::from([
            ("count".to_owned(), owned(ZValue::from(4u32))),
            ("label_configurable".to_owned(), owned(ZValue::from(false))),
        ]);
        let capabilities: HashMap<String, OwnedValue> = HashMap::from([
            (
                "resolutions".to_owned(),
                owned(ZValue::from(vec![300u32, 600])),
            ),
            (
                "color_modes".to_owned(),
                owned(ZValue::from(vec!["color", "gray"])),
            ),
            (
                "sources".to_owned(),
                owned(ZValue::from(vec!["flatbed", "adf"])),
            ),
            ("duplex".to_owned(), owned(ZValue::from(true))),
            ("buttons".to_owned(), owned(ZValue::from(buttons))),
        ]);

        HashMap::from([
            (
                "Id".to_owned(),
                owned(ZValue::from("brother_net_192_2E168_2E1_2E23")),
            ),
            ("Name".to_owned(), owned(ZValue::from("MFC-L2710DW"))),
            (
                "Backend".to_owned(),
                owned(ZValue::from("proprietary:brother")),
            ),
            ("Address".to_owned(), owned(ZValue::from("192.168.1.23"))),
            ("Capabilities".to_owned(), owned(ZValue::from(capabilities))),
            (
                "SupportedProfiles".to_owned(),
                owned(ZValue::from(vec!["image", "document"])),
            ),
            ("Paired".to_owned(), owned(ZValue::from(true))),
            ("Connected".to_owned(), owned(ZValue::from(false))),
            ("Status".to_owned(), owned(ZValue::from("online"))),
            ("DefaultProfile".to_owned(), owned(ZValue::from("document"))),
            ("PairingState".to_owned(), owned(ZValue::from("done"))),
            ("PairingError".to_owned(), owned(ZValue::from(""))),
            (
                "PairingInfo".to_owned(),
                owned(ZValue::from(HashMap::<String, OwnedValue>::new())),
            ),
        ])
    }

    #[test]
    fn a_get_all_reply_decodes_into_the_model() {
        let state = ScannerState::from_properties(&brother()).unwrap();

        assert_eq!(state.id.as_str(), "brother_net_192_2E168_2E1_2E23");
        assert_eq!(state.name, "MFC-L2710DW");
        assert_eq!(state.backend, "proprietary:brother");
        assert_eq!(state.address, "192.168.1.23");
        assert_eq!(
            state.capabilities.color_modes,
            [ColorMode::Color, ColorMode::Gray]
        );
        assert_eq!(state.capabilities.sources, [Source::Flatbed, Source::Adf]);
        assert_eq!(state.capabilities.button_count(), 4);
        assert_eq!(
            state.supported_profiles,
            [ProfileKind::Image, ProfileKind::Document]
        );
        assert!(state.paired);
        assert!(!state.connected);
        assert_eq!(state.status, Status::Online);
        assert_eq!(state.default_profile, Some(ProfileKind::Document));
        assert_eq!(state.pairing, PairingState::Done);
    }

    /// `""` is unassigned, not an invalid profile (§5).
    #[test]
    fn an_empty_default_profile_is_none() {
        let mut properties = brother();
        properties.insert("DefaultProfile".to_owned(), owned(ZValue::from("")));

        assert_eq!(
            ScannerState::from_properties(&properties)
                .unwrap()
                .default_profile,
            None
        );
    }

    /// A property §3 requires, missing: a daemon serving something else under our name.
    #[test]
    fn a_missing_property_names_itself() {
        let mut properties = brother();
        properties.remove("Status");

        assert_eq!(
            ScannerState::from_properties(&properties).unwrap_err(),
            DecodeError::Missing("Status".to_owned())
        );
    }

    /// A closed set stays closed, and the message says which value broke it.
    #[test]
    fn a_status_outside_the_documented_five_is_refused() {
        let mut properties = brother();
        properties.insert("Status".to_owned(), owned(ZValue::from("sleeping")));

        let error = ScannerState::from_properties(&properties).unwrap_err();
        assert!(error.to_string().contains("sleeping"), "{error}");
    }

    /// The delta a `PropertiesChanged` carries lands on the state, and nothing else moves.
    #[test]
    fn a_partial_change_yields_a_whole_state() {
        let mut state = ScannerState::from_properties(&brother()).unwrap();

        state
            .apply(&HashMap::from([("Status", ZValue::from("busy"))]))
            .unwrap();

        assert_eq!(state.status, Status::Busy);
        assert_eq!(state.name, "MFC-L2710DW", "nothing else may move");
        assert_eq!(state.pairing, PairingState::Done);
    }

    /// The pair of properties that describe one fact, arriving together and apart.
    #[test]
    fn the_pairing_pair_is_rebuilt_from_whichever_half_arrives() {
        let mut state = ScannerState::from_properties(&brother()).unwrap();

        // Both together, as the daemon sends a failure.
        state
            .apply(&HashMap::from([
                ("PairingState", ZValue::from("failed")),
                ("PairingError", ZValue::from("404 fetching brscan5")),
                ("Paired", ZValue::from(false)),
            ]))
            .unwrap();
        assert_eq!(
            state.pairing,
            PairingState::Failed("404 fetching brscan5".to_owned())
        );
        assert!(!state.paired);

        // The message alone: a retry that failed differently is a real change to
        // `PairingError` and to nothing else.
        state
            .apply(&HashMap::from([(
                "PairingError",
                ZValue::from("no route to host"),
            )]))
            .unwrap();
        assert_eq!(
            state.pairing,
            PairingState::Failed("no route to host".to_owned())
        );

        // And the state alone clears the message, because the invariant lives in the
        // type rather than in whoever remembers to blank the other property.
        state
            .apply(&HashMap::from([("PairingState", ZValue::from("pairing"))]))
            .unwrap();
        assert_eq!(state.pairing, PairingState::Pairing);
        assert_eq!(state.pairing.pairing_error(), "");
    }

    /// A daemon newer than this client must not stop it following what it does know.
    #[test]
    fn an_unknown_property_is_ignored() {
        let mut state = ScannerState::from_properties(&brother()).unwrap();

        state
            .apply(&HashMap::from([
                ("FirmwareVersion", ZValue::from("1.2.3")),
                ("Status", ZValue::from("error")),
            ]))
            .unwrap();

        assert_eq!(state.status, Status::Error);
    }

    /// `Id` is the path element, so a signal claiming it moved is not a change to
    /// follow — it is a different object.
    #[test]
    fn an_id_that_changes_is_refused_rather_than_followed() {
        let mut state = ScannerState::from_properties(&brother()).unwrap();
        let before = state.clone();

        assert!(matches!(
            state.apply(&HashMap::from([("Id", ZValue::from("something_else"))])),
            Err(DecodeError::Type { key, .. }) if key == "Id"
        ));
        assert_eq!(state.id, before.id);
    }
}
