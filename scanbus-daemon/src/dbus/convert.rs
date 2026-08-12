//! The model, rendered onto the wire.
//!
//! `scanbus-core` is typed where D-Bus is not, and says so: `Capabilities` is a struct
//! with fields rather than the `a{sv}` of [`scanbus-dbus-api.md`] §3, and
//! [`scanbus_core::Value`] is deliberately not `zvariant::Value` because core must build
//! without a bus. This module is the one place the two meet, so that the D-Bus
//! signatures of the model are decided once instead of at each property getter.
//!
//! # Both directions, and why the inbound one is lossy on purpose
//!
//! `Button1.ProfileOptions` (2.5) is the first property a client may *write*, so the
//! inverse of [`value`] lands here as [`from_value`]. It is deliberately not a bijection:
//! D-Bus has eight integer types and [`Value`] has two, so `y`, `n`, `q`, `i` and `u` all
//! widen into [`Value::U64`] or [`Value::I64`]. Widening rather than refusing is the
//! right call for a map whose keys this daemon does not define — a client that sends
//! `{"quality": <uint16 80>}` means 80 — and the round trip is still *semantically*
//! stable, which is what the pairing store (4.1) needs of it.
//!
//! What is refused is what has no home in the model at all: `h` (a file descriptor,
//! which [`Value`] cannot express and which must not be silently dropped from a config
//! file), structs, and dictionaries keyed by anything but a string.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md

use std::collections::{BTreeMap, HashMap};

use scanbus_core::{Capabilities, Value};
use thiserror::Error;
use zbus::zvariant::{OwnedValue, Value as ZValue};

use crate::profiles::options::SchemaEntry;

/// A `a{sv}` map, the shape every open-ended property in §3 has.
pub type Dict = HashMap<String, OwnedValue>;

/// Renders a [`Value`] as the variant body a client receives.
///
/// The integer variants keep the type core chose — `U64` becomes `t`, `I64` becomes `x`
/// — rather than being collapsed into one: a client reading `Capabilities` gets the
/// signature the backend meant, and a round-trip through the pairing store (4.1) does
/// not silently re-type anything.
///
/// `Array` becomes `av` rather than a homogeneous array, because [`Value::Array`] has no
/// element-type invariant to build one from: `[1, "a"]` is representable in core and has
/// to stay representable here.
pub fn value(value: &Value) -> OwnedValue {
    let rendered = match value {
        Value::Bool(v) => ZValue::from(*v),
        Value::U64(v) => ZValue::from(*v),
        Value::I64(v) => ZValue::from(*v),
        Value::F64(v) => ZValue::from(*v),
        Value::Str(v) => ZValue::from(v.as_str()),
        Value::Array(items) => ZValue::from(
            items
                .iter()
                .map(|item| ZValue::from(self::value(item)))
                .collect::<Vec<_>>(),
        ),
        Value::Dict(entries) => ZValue::from(dict(entries.iter().map(|(k, v)| (k.clone(), v)))),
    };

    // Infallible for everything built above: `try_to_owned` only fails on a value
    // holding a file descriptor, which core's `Value` cannot express.
    OwnedValue::try_from(rendered).expect("a rendered core Value never holds a file descriptor")
}

/// Renders `(key, value)` pairs as the `a{sv}` a property getter returns.
pub fn dict<'values>(entries: impl IntoIterator<Item = (String, &'values Value)>) -> Dict {
    entries
        .into_iter()
        .map(|(key, item)| (key, value(item)))
        .collect()
}

/// A small `a{sv}` of plain strings — `PairingInfo`'s shape, `{"code": "482913"}` or
/// empty.
pub fn str_map(entries: &[(&str, &str)]) -> Dict {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), owned(ZValue::from(*value))))
        .collect()
}

/// Why an `a{sv}` a client sent could not be read into the model.
///
/// Both variants name the signature that caused it, because the client that sent it is
/// looking at its own source and not at ours: "`ProfileOptions` cannot carry a value of
/// signature `(ss)`" is actionable, "invalid arguments" is not.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FromValueError {
    /// A type the model has no variant for: a struct, a file descriptor, a `maybe`.
    #[error("a value of signature {0} has no equivalent in this daemon's model")]
    Unrepresentable(String),

    /// A dictionary keyed by something other than a string.
    ///
    /// Every map in the API is `a{sv}`, and [`Value::Dict`] is keyed by [`String`], so a
    /// `a{iv}` nested inside an option would have to lose or invent its keys.
    #[error("dictionary keys must be strings; got a key of signature {0}")]
    DictKey(String),
}

/// Reads a variant a client sent into the model's own [`Value`].
///
/// Nested variants are unwrapped: a client that builds `<<"~/Scans">>` and one that
/// builds `<"~/Scans">` mean the same thing, and the difference is an artefact of how
/// the map was assembled rather than something a config file should record.
///
/// # Errors
///
/// [`FromValueError`] for anything the model cannot hold — see the module documentation
/// on what widens and what is refused.
pub fn from_value(value: &ZValue<'_>) -> Result<Value, FromValueError> {
    Ok(match value {
        ZValue::Bool(v) => Value::Bool(*v),
        ZValue::U8(v) => Value::U64((*v).into()),
        ZValue::U16(v) => Value::U64((*v).into()),
        ZValue::U32(v) => Value::U64((*v).into()),
        ZValue::U64(v) => Value::U64(*v),
        ZValue::I16(v) => Value::I64((*v).into()),
        ZValue::I32(v) => Value::I64((*v).into()),
        ZValue::I64(v) => Value::I64(*v),
        ZValue::F64(v) => Value::F64(*v),
        ZValue::Str(v) => Value::Str(v.as_str().to_owned()),
        ZValue::ObjectPath(v) => Value::Str(v.as_str().to_owned()),
        ZValue::Signature(v) => Value::Str(v.to_string()),
        // The unwrapping above: `v` inside `v`.
        ZValue::Value(inner) => from_value(inner)?,
        ZValue::Array(items) => Value::Array(
            items
                .iter()
                .map(from_value)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        ZValue::Dict(entries) => {
            let mut map = BTreeMap::new();
            for (key, item) in entries.iter() {
                let key = match from_value(key)? {
                    Value::Str(key) => key,
                    _ => {
                        return Err(FromValueError::DictKey(key.value_signature().to_string()));
                    }
                };
                map.insert(key, from_value(item)?);
            }
            Value::Dict(map)
        }
        other => {
            return Err(FromValueError::Unrepresentable(
                other.value_signature().to_string(),
            ));
        }
    })
}

/// Reads a whole `a{sv}` argument into the [`BTreeMap`] the model and the backend seam
/// both use.
///
/// [`BTreeMap`] and not the [`HashMap`] the wire arrives as, to match
/// [`ButtonInfo::profile_options`](scanbus_core::ButtonInfo::profile_options): a vendor
/// config file this ends up in is one a user may also read, so the key order has to be
/// stable rather than whatever the client's hasher produced.
///
/// # Errors
///
/// [`FromValueError`] for the first value that has no equivalent in the model. The map
/// is then not partially converted — the caller gets nothing and writes nothing.
pub fn from_dict(dict: &Dict) -> Result<BTreeMap<String, Value>, FromValueError> {
    dict.iter()
        .map(|(key, value)| Ok((key.clone(), from_value(value)?)))
        .collect()
}

/// Renders [`Capabilities`] as the `Capabilities` property of §3.
///
/// The documented keys are always present, including the empty and `false` ones: a
/// client branching on `duplex` should read `false`, not have to tell "absent" from "not
/// supported". [`Capabilities::extra`] — the keys a backend reported that this version
/// has no field for — is merged in underneath, so a known key always wins over a
/// backend's homonym rather than the merge order deciding it.
///
/// `profiles` is the one exception to "always present": it is what the *device* said it
/// can produce (9.3), and rendering an empty array for every scanner that never had an
/// opinion would read as "produces nothing" rather than "did not say". Absent means did
/// not say; `SupportedProfiles` is the property that always answers.
pub fn capabilities(capabilities: &Capabilities) -> Dict {
    let mut rendered = dict(
        capabilities
            .extra
            .iter()
            .map(|(key, value)| (key.clone(), value)),
    );

    rendered.insert(
        "resolutions".to_owned(),
        owned(ZValue::from(capabilities.resolutions.clone())),
    );
    rendered.insert(
        "color_modes".to_owned(),
        owned(ZValue::from(
            capabilities
                .color_modes
                .iter()
                .map(|mode| match mode {
                    scanbus_core::ColorMode::Color => "color",
                    scanbus_core::ColorMode::Gray => "gray",
                    scanbus_core::ColorMode::Bw => "bw",
                })
                .collect::<Vec<_>>(),
        )),
    );
    rendered.insert(
        "sources".to_owned(),
        owned(ZValue::from(
            capabilities
                .sources
                .iter()
                .map(|source| match source {
                    scanbus_core::Source::Flatbed => "flatbed",
                    scanbus_core::Source::Adf => "adf",
                })
                .collect::<Vec<_>>(),
        )),
    );
    rendered.insert(
        "duplex".to_owned(),
        owned(ZValue::from(capabilities.duplex)),
    );

    let buttons: Dict = HashMap::from([
        (
            "count".to_owned(),
            owned(ZValue::from(capabilities.buttons.count)),
        ),
        (
            "label_configurable".to_owned(),
            owned(ZValue::from(capabilities.buttons.label_configurable)),
        ),
    ]);
    rendered.insert("buttons".to_owned(), owned(ZValue::from(buttons)));

    if !capabilities.profiles.is_empty() {
        rendered.insert(
            "profiles".to_owned(),
            owned(ZValue::from(
                capabilities
                    .profiles
                    .iter()
                    .map(|kind| kind.as_str())
                    .collect::<Vec<_>>(),
            )),
        );
    }

    rendered
}

/// Renders the resolved option table as the `OptionsSchema` property of §6.
///
/// The entries go through here rather than through [`dict`] because §6 fixes their
/// signatures and [`Value`] cannot express one of them: `values` is `as`, and a
/// [`Value::Array`] renders as `av` by construction — right for an option map whose
/// element types this daemon does not decide, wrong for a list that is strings and only
/// ever strings. The typed fields of [`SchemaEntry`] are what let this be `as` without
/// weakening [`value`].
///
/// `min` and `max` are emitted together or not at all, so a client never has to handle a
/// half-bounded range that no declaration can produce.
pub fn options_schema(entries: &[SchemaEntry]) -> Dict {
    entries
        .iter()
        .map(|entry| {
            let mut rendered = Dict::from([
                ("type".to_owned(), owned(ZValue::from(entry.type_name))),
                ("default".to_owned(), value(&entry.default)),
                (
                    "description".to_owned(),
                    owned(ZValue::from(entry.description)),
                ),
            ]);

            if !entry.values.is_empty() {
                rendered.insert(
                    "values".to_owned(),
                    owned(ZValue::from(entry.values.to_vec())),
                );
            }

            if let (Some(min), Some(max)) = (&entry.min, &entry.max) {
                rendered.insert("min".to_owned(), value(min));
                rendered.insert("max".to_owned(), value(max));
            }

            (entry.key.to_owned(), owned(ZValue::from(rendered)))
        })
        .collect()
}

/// Same infallibility as in [`value`], for the values built from typed fields.
fn owned(value: ZValue<'_>) -> OwnedValue {
    OwnedValue::try_from(value).expect("a value built from a typed field holds no file descriptor")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use scanbus_core::{ButtonsCapability, ColorMode, Source};

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
            profiles: Vec::new(),
            extra: BTreeMap::new(),
        }
    }

    /// The example the API doc spells out, key by key.
    #[test]
    fn capabilities_render_the_documented_example() {
        let rendered = capabilities(&brother_mfc());

        assert_eq!(
            Vec::<u32>::try_from(rendered["resolutions"].clone()).unwrap(),
            [100, 200, 300, 600]
        );
        assert_eq!(
            Vec::<String>::try_from(rendered["color_modes"].clone()).unwrap(),
            ["color", "gray", "bw"]
        );
        assert_eq!(
            Vec::<String>::try_from(rendered["sources"].clone()).unwrap(),
            ["flatbed", "adf"]
        );
        assert!(bool::try_from(rendered["duplex"].clone()).unwrap());

        let buttons = Dict::try_from(rendered["buttons"].clone()).unwrap();
        assert_eq!(u32::try_from(buttons["count"].clone()).unwrap(), 4);
        assert!(!bool::try_from(buttons["label_configurable"].clone()).unwrap());
    }

    /// A scanner that reports nothing still renders every documented key, so a client
    /// never has to tell "absent" from "not supported".
    #[test]
    fn an_empty_capability_set_still_carries_every_key() {
        let rendered = capabilities(&Capabilities::default());

        for key in ["resolutions", "color_modes", "sources", "duplex", "buttons"] {
            assert!(rendered.contains_key(key), "{key} is missing: {rendered:?}");
        }
        assert!(
            Vec::<u32>::try_from(rendered["resolutions"].clone())
                .unwrap()
                .is_empty()
        );
    }

    /// Keys this version has no field for reach the client rather than being dropped —
    /// and cannot shadow a key that does have one.
    #[test]
    fn extra_keys_survive_and_never_shadow_a_known_one() {
        let capabilities_with_extras = Capabilities {
            extra: BTreeMap::from([
                (
                    "max_scan_area_mm".to_owned(),
                    Value::Array(vec![Value::U64(216), Value::U64(356)]),
                ),
                ("duplex".to_owned(), Value::Bool(false)),
            ]),
            ..brother_mfc()
        };

        let rendered = capabilities(&capabilities_with_extras);
        assert!(rendered.contains_key("max_scan_area_mm"));
        assert!(
            bool::try_from(rendered["duplex"].clone()).unwrap(),
            "the typed field must win over a backend's homonym"
        );
    }

    /// Every shape a `ProfileOptions` write can carry survives the trip in, and the
    /// integer widening is the documented lossy part.
    #[test]
    fn a_written_dict_is_read_back_into_the_model() {
        let options = Dict::from([
            ("multipage".to_owned(), owned(ZValue::from(true))),
            ("quality".to_owned(), owned(ZValue::from(80u16))),
            ("offset".to_owned(), owned(ZValue::from(-1i32))),
            ("gamma".to_owned(), owned(ZValue::from(2.2f64))),
            (
                "output_folder".to_owned(),
                owned(ZValue::from("~/Scans/invoices")),
            ),
            (
                "resolutions".to_owned(),
                owned(ZValue::from(vec![300u32, 600])),
            ),
        ]);

        let read = from_dict(&options).unwrap();

        assert_eq!(read["multipage"], Value::Bool(true));
        // `q` and `u` both widen into the one unsigned variant core has.
        assert_eq!(read["quality"], Value::U64(80));
        assert_eq!(read["offset"], Value::I64(-1));
        assert_eq!(read["gamma"], Value::F64(2.2));
        assert_eq!(
            read["output_folder"],
            Value::Str("~/Scans/invoices".to_owned())
        );
        assert_eq!(
            read["resolutions"],
            Value::Array(vec![Value::U64(300), Value::U64(600)])
        );

        // And what came back out is what a client reads off the property afterwards.
        let rendered = dict(read.iter().map(|(key, value)| (key.clone(), value)));
        assert_eq!(from_dict(&rendered).unwrap(), read);
    }

    /// A client that wraps its values twice means the same thing as one that does not.
    ///
    /// [`ZValue::Value`] is built by hand here: `ZValue::from(ZValue)` is the identity, so
    /// the only way to get a genuine `<<…>>` is to nest the variant explicitly — which is
    /// also the shape a client assembling its map one variant at a time produces.
    #[test]
    fn a_nested_variant_is_unwrapped() {
        let nested = owned(ZValue::Value(Box::new(ZValue::from("~/Scans"))));
        assert_eq!(
            nested.value_signature().to_string(),
            "v",
            "the test has to nest a variant to be about nesting at all"
        );
        assert_eq!(
            from_value(&nested).unwrap(),
            Value::Str("~/Scans".to_owned())
        );

        let dict = owned(ZValue::from(Dict::from([(
            "folder".to_owned(),
            owned(ZValue::from("~/Scans")),
        )])));
        assert_eq!(
            from_value(&dict).unwrap(),
            Value::Dict(BTreeMap::from([(
                "folder".to_owned(),
                Value::Str("~/Scans".to_owned())
            )]))
        );
    }

    /// What has no home in the model is refused rather than dropped: an option silently
    /// lost is a config file that does not say what the client asked for.
    #[test]
    fn a_value_the_model_cannot_hold_is_refused_by_signature() {
        let structure = owned(ZValue::from((1u32, "two")));
        let error = from_value(&structure).expect_err("a struct has no Value variant");
        assert_eq!(
            error,
            FromValueError::Unrepresentable("(us)".to_owned()),
            "{error}"
        );

        let mut keyed_by_int = zbus::zvariant::Dict::new(
            &<i32 as zbus::zvariant::Type>::SIGNATURE.clone(),
            &<ZValue<'_> as zbus::zvariant::Type>::SIGNATURE.clone(),
        );
        keyed_by_int.add(1i32, ZValue::from("one")).unwrap();
        let error = from_value(&ZValue::from(keyed_by_int))
            .expect_err("every map in the API is keyed by strings");
        assert_eq!(error, FromValueError::DictKey("i".to_owned()), "{error}");
    }

    /// §6's shape, on the wire: an entry per option, `values` as `as` rather than the
    /// `av` a [`Value::Array`] would have produced, and bounds only where they exist.
    #[test]
    fn a_schema_entry_has_the_signatures_the_api_fixes() {
        let entries = crate::profiles::options::schema(
            scanbus_core::ProfileKind::Image,
            std::path::Path::new("/home/user/Pictures/scanbus/image"),
        );
        let rendered = options_schema(&entries);

        let format = Dict::try_from(rendered["format"].clone()).unwrap();
        assert_eq!(String::try_from(format["type"].clone()).unwrap(), "string");
        assert_eq!(String::try_from(format["default"].clone()).unwrap(), "jpeg");
        assert_eq!(
            format["values"].value_signature().to_string(),
            "as",
            "a client decoding the documented signature has to find it"
        );
        assert_eq!(
            Vec::<String>::try_from(format["values"].clone()).unwrap(),
            ["jpeg", "jpg", "png"]
        );
        assert!(
            !String::try_from(format["description"].clone())
                .unwrap()
                .is_empty()
        );
        assert!(!format.contains_key("min"), "a string option has no bounds");

        let quality = Dict::try_from(rendered["quality"].clone()).unwrap();
        assert_eq!(
            String::try_from(quality["type"].clone()).unwrap(),
            "integer"
        );
        assert_eq!(u64::try_from(quality["min"].clone()).unwrap(), 1);
        assert_eq!(u64::try_from(quality["max"].clone()).unwrap(), 100);
        assert!(
            !quality.contains_key("values"),
            "a bounded integer is not a closed set"
        );

        let folder = Dict::try_from(rendered["output_folder"].clone()).unwrap();
        assert_eq!(String::try_from(folder["type"].clone()).unwrap(), "path");
        assert_eq!(
            String::try_from(folder["default"].clone()).unwrap(),
            "/home/user/Pictures/scanbus/image",
            "the resolved directory, not the empty stored value"
        );
    }

    /// The integer variants keep the signature core chose for them.
    #[test]
    fn values_keep_their_signatures() {
        assert_eq!(value(&Value::U64(1)).value_signature().to_string(), "t");
        assert_eq!(value(&Value::I64(-1)).value_signature().to_string(), "x");
        assert_eq!(value(&Value::F64(2.2)).value_signature().to_string(), "d");
        assert_eq!(value(&Value::Bool(true)).value_signature().to_string(), "b");
        assert_eq!(
            value(&Value::Str(String::new()))
                .value_signature()
                .to_string(),
            "s"
        );
        // Heterogeneous by construction, hence `av` rather than a typed array.
        assert_eq!(
            value(&Value::Array(vec![
                Value::U64(1),
                Value::Str("a".to_owned())
            ]))
            .value_signature()
            .to_string(),
            "av"
        );
        assert_eq!(
            value(&Value::Dict(BTreeMap::new()))
                .value_signature()
                .to_string(),
            "a{sv}"
        );
    }
}
