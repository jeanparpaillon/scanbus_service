//! The wire's `a{sv}`, read back into the model.
//!
//! The daemon renders the model onto the wire (`scanbus_daemon::dbus::convert`); this is
//! the other direction, and the two are inverses rather than copies. What is duplicated
//! is deliberately small: [`to_variant`] exists because a client *sends* `a{sv}` too —
//! `Pair()` options, a `Button1.ProfileOptions` write — and nothing else here has a
//! counterpart on the daemon side.
//!
//! # What "unknown" means, key by key
//!
//! [`capabilities`] keeps a key it has no field for in [`Capabilities::extra`] rather
//! than dropping it: a backend that reports `max_scan_area_mm` through a daemon newer
//! than this client should reach `scanbus show`, not vanish between the two. The same
//! openness does *not* extend to the closed sets §3 fixes — a `color_modes` entry that
//! is not `color`, `gray` or `bw` is refused, because the alternative is a scanner whose
//! ADF quietly is not offered on account of a spelling.
//!
//! # Integers widen, and that is not free
//!
//! D-Bus has six integer signatures where [`Value`] has two. Reading `u8`/`u16`/`u32`
//! into [`Value::U64`] and `i16`/`i32` into [`Value::I64`] means a value that went out as
//! `y` comes back as `t`, so `to_variant(from_variant(v)) != v` for those. The round-trip
//! that matters is the other one — a [`Value`] this crate sends and reads back is
//! unchanged — and it is what the tests pin.

use std::collections::{BTreeMap, HashMap};

use scanbus_core::{ButtonsCapability, Capabilities, ColorMode, Source, Value};
use zbus::zvariant::{OwnedValue, Value as ZValue};

/// A `a{sv}` map, the shape every open-ended property in §3 has.
pub type Dict = HashMap<String, OwnedValue>;

/// A value on the bus that this version cannot turn into the model.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// A D-Bus type the model has no representation for.
    #[error("a value of signature {signature} has no equivalent in the model")]
    Signature {
        /// The signature that arrived.
        signature: String,
    },

    /// A documented key whose value is not of its documented type.
    #[error("{key:?} should be {expected}, but arrived as {signature}")]
    Type {
        /// The key, as §3 spells it.
        key: String,
        /// What §3 says it holds.
        expected: &'static str,
        /// What actually arrived.
        signature: String,
    },

    /// A member of a closed set that §3 does not define.
    #[error("{key:?} contains {value:?}, which is not one of [{expected}]")]
    Member {
        /// The key the value came under.
        key: String,
        /// The value that is not in the set.
        value: String,
        /// The set §3 fixes, comma-separated.
        expected: &'static str,
    },

    /// A property an interface must have, that the daemon did not send.
    #[error("{0} is missing from the reply")]
    Missing(String),

    /// A string property that is not one of the documented values.
    #[error(transparent)]
    Parse(#[from] scanbus_core::ParseError),

    /// An `Id` the daemon sent that is not a legal one.
    ///
    /// Its own variant rather than a [`DecodeError::Parse`]: an id outside the charset
    /// cannot have come from a path this daemon exported, so the interesting fact is
    /// which string arrived, not which type refused it.
    #[error(transparent)]
    Id(#[from] scanbus_core::InvalidScannerId),
}

/// Renders a [`Value`] as the variant a client sends.
///
/// The signatures core chose are kept — `U64` becomes `t`, `I64` becomes `x` — and an
/// [`Value::Array`] becomes `av` rather than a homogeneous array, because `[1, "a"]` is
/// representable in core and has to stay representable here. This mirrors the daemon's
/// renderer exactly; anything else would make a value the daemon accepts from itself but
/// not from the CLI.
pub fn to_variant(value: &Value) -> OwnedValue {
    let rendered = match value {
        Value::Bool(v) => ZValue::from(*v),
        Value::U64(v) => ZValue::from(*v),
        Value::I64(v) => ZValue::from(*v),
        Value::F64(v) => ZValue::from(*v),
        Value::Str(v) => ZValue::from(v.as_str()),
        Value::Array(items) => ZValue::from(
            items
                .iter()
                .map(|item| ZValue::from(to_variant(item)))
                .collect::<Vec<_>>(),
        ),
        Value::Dict(entries) => ZValue::from(to_dict(entries)),
    };

    // Infallible for everything built above: `try_to_owned` only fails on a value holding
    // a file descriptor, which core's `Value` cannot express.
    OwnedValue::try_from(rendered).expect("a rendered core Value never holds a file descriptor")
}

/// Renders a map of [`Value`]s as the `a{sv}` a method argument or a property write takes.
pub fn to_dict(entries: &BTreeMap<String, Value>) -> Dict {
    entries
        .iter()
        .map(|(key, value)| (key.clone(), to_variant(value)))
        .collect()
}

/// Reads a variant off the bus into the model's open-ended [`Value`].
///
/// # Errors
///
/// [`DecodeError::Signature`] for a type the model cannot hold: a struct, a file
/// descriptor, or a dictionary whose keys are not strings. None of those appear anywhere
/// in §3–§6, so meeting one means talking to something that is not this API.
pub fn from_variant(value: &ZValue<'_>) -> Result<Value, DecodeError> {
    Ok(match value {
        ZValue::Bool(v) => Value::Bool(*v),
        // The five narrower integer signatures widen: the model has one unsigned and one
        // signed integer, and refusing a `u32` for not being a `u64` would reject
        // `Capabilities.resolutions`, which is exactly `au`.
        ZValue::U8(v) => Value::U64((*v).into()),
        ZValue::U16(v) => Value::U64((*v).into()),
        ZValue::U32(v) => Value::U64((*v).into()),
        ZValue::U64(v) => Value::U64(*v),
        ZValue::I16(v) => Value::I64((*v).into()),
        ZValue::I32(v) => Value::I64((*v).into()),
        ZValue::I64(v) => Value::I64(*v),
        ZValue::F64(v) => Value::F64(*v),
        ZValue::Str(v) => Value::Str(v.as_str().to_owned()),
        // A path is a string with a charset invariant the model does not track; it is
        // still readable, which is what `Job1.Result` needs when it names an object.
        ZValue::ObjectPath(v) => Value::Str(v.as_str().to_owned()),
        ZValue::Signature(v) => Value::Str(v.to_string()),
        // `v` nested in `av` or `a{sv}`: one unwrap, and whatever is inside decides.
        ZValue::Value(inner) => from_variant(inner)?,
        ZValue::Array(items) => Value::Array(
            items
                .iter()
                .map(from_variant)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        ZValue::Dict(entries) => {
            let mut decoded = BTreeMap::new();
            for (key, value) in entries.iter() {
                let ZValue::Str(key) = key else {
                    return Err(DecodeError::Signature {
                        signature: value.value_signature().to_string(),
                    });
                };
                decoded.insert(key.as_str().to_owned(), from_variant(value)?);
            }
            Value::Dict(decoded)
        }
        other => {
            return Err(DecodeError::Signature {
                signature: other.value_signature().to_string(),
            });
        }
    })
}

/// Reads a whole `a{sv}` into the model — `Button1.ProfileOptions`, `Job1.Result`.
///
/// Neither has a typed model to land in: `ProfileOptions` is open by design (§5), and
/// `Job1.Result` is profile-specific (§6), with the shapes that document it belonging to
/// the profile processors of workstream 3. A map of [`Value`]s is what both are, and it
/// is what [`scanbus_core::ButtonInfo::profile_options`] already holds.
///
/// # Errors
///
/// [`DecodeError::Signature`], from [`from_variant`], for any value the model cannot hold.
pub fn from_dict(dict: &Dict) -> Result<BTreeMap<String, Value>, DecodeError> {
    dict.iter()
        .map(|(key, value)| Ok((key.clone(), from_variant(value)?)))
        .collect()
}

/// Reads the `Capabilities` property of §3 into [`Capabilities`].
///
/// # Errors
///
/// [`DecodeError::Type`] for a documented key of the wrong type, [`DecodeError::Member`]
/// for a `color_modes` or `sources` entry outside the sets §3 fixes, and whatever
/// [`from_variant`] refuses for the rest. An absent key is not an error: the daemon sends
/// every one of them (its renderer says so), but a `Capabilities` short of `duplex` is
/// still a scanner a client can list, and [`Capabilities`] has a default for each.
pub fn capabilities(dict: &Dict) -> Result<Capabilities, DecodeError> {
    let mut extra = from_dict(dict)?;

    let resolutions = match extra.remove("resolutions") {
        None => Vec::new(),
        Some(value) => integer_array(&value, "resolutions")?,
    };
    let color_modes = match extra.remove("color_modes") {
        None => Vec::new(),
        Some(value) => string_array(&value, "color_modes")?
            .iter()
            .map(|mode| match mode.as_str() {
                "color" => Ok(ColorMode::Color),
                "gray" => Ok(ColorMode::Gray),
                "bw" => Ok(ColorMode::Bw),
                other => Err(DecodeError::Member {
                    key: "color_modes".to_owned(),
                    value: other.to_owned(),
                    expected: "color, gray, bw",
                }),
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    let sources = match extra.remove("sources") {
        None => Vec::new(),
        Some(value) => string_array(&value, "sources")?
            .iter()
            .map(|source| match source.as_str() {
                "flatbed" => Ok(Source::Flatbed),
                "adf" => Ok(Source::Adf),
                other => Err(DecodeError::Member {
                    key: "sources".to_owned(),
                    value: other.to_owned(),
                    expected: "flatbed, adf",
                }),
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    let duplex = match extra.remove("duplex") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| typed("duplex", "b", &value))?,
    };
    let buttons = match extra.remove("buttons") {
        None => ButtonsCapability::default(),
        Some(value) => {
            let entries = value
                .as_dict()
                .ok_or_else(|| typed("buttons", "a{sv}", &value))?;
            ButtonsCapability {
                count: match entries.get("count") {
                    None => 0,
                    Some(count) => count
                        .as_u64()
                        .and_then(|count| u32::try_from(count).ok())
                        .ok_or_else(|| typed("buttons.count", "u", count))?,
                },
                label_configurable: match entries.get("label_configurable") {
                    None => false,
                    Some(flag) => flag
                        .as_bool()
                        .ok_or_else(|| typed("buttons.label_configurable", "b", flag))?,
                },
            }
        }
    };
    let profiles = match extra.remove("profiles") {
        None => Vec::new(),
        Some(value) => string_array(&value, "profiles")?
            .iter()
            .map(|profile| profile.parse())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| DecodeError::Member {
                key: "profiles".to_owned(),
                value: "<invalid>".to_owned(),
                expected: "image, document, email, ocr",
            })?,
    };

    // Whatever is left is a key this version has no field for — kept, not dropped.
    Ok(Capabilities {
        resolutions,
        color_modes,
        sources,
        duplex,
        buttons,
        profiles,
        extra,
    })
}

/// The `au` of `resolutions`, from either the typed array or an `av` of integers.
fn integer_array(value: &Value, key: &'static str) -> Result<Vec<u32>, DecodeError> {
    value
        .as_array()
        .ok_or_else(|| typed(key, "au", value))?
        .iter()
        .map(|item| {
            item.as_u64()
                .and_then(|item| u32::try_from(item).ok())
                .ok_or_else(|| typed(key, "au", value))
        })
        .collect()
}

/// The `as` of `color_modes` / `sources`, from either the typed array or an `av`.
///
/// Both spellings reach a client in practice — the daemon sends `as`, a backend that
/// built its variants one at a time would send `av` — and the difference is invisible in
/// the source of whoever sent it, so refusing one would be a distinction nobody can act
/// on. [`from_variant`] has already flattened `av` into the same [`Value::Array`], which
/// is why this needs no second branch.
fn string_array(value: &Value, key: &'static str) -> Result<Vec<String>, DecodeError> {
    value
        .as_array()
        .ok_or_else(|| typed(key, "as", value))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| typed(key, "as", value))
        })
        .collect()
}

/// A [`DecodeError::Type`] naming what arrived, in the model's vocabulary.
fn typed(key: &str, expected: &'static str, got: &Value) -> DecodeError {
    let signature = match got {
        Value::Bool(_) => "b",
        Value::U64(_) => "t",
        Value::I64(_) => "x",
        Value::F64(_) => "d",
        Value::Str(_) => "s",
        Value::Array(_) => "av",
        Value::Dict(_) => "a{sv}",
    };

    DecodeError::Type {
        key: key.to_owned(),
        expected,
        signature: signature.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(value: ZValue<'_>) -> OwnedValue {
        OwnedValue::try_from(value).unwrap()
    }

    /// The `Capabilities` example §3 spells out, in the signatures the daemon sends it
    /// with: `au`, `as`, `b`, and a nested `a{sv}`.
    fn documented_example() -> Dict {
        let buttons: HashMap<String, OwnedValue> = HashMap::from([
            ("count".to_owned(), owned(ZValue::from(4u32))),
            ("label_configurable".to_owned(), owned(ZValue::from(false))),
        ]);

        HashMap::from([
            (
                "resolutions".to_owned(),
                owned(ZValue::from(vec![100u32, 200, 300, 600])),
            ),
            (
                "color_modes".to_owned(),
                owned(ZValue::from(vec!["color", "gray", "bw"])),
            ),
            (
                "sources".to_owned(),
                owned(ZValue::from(vec!["flatbed", "adf"])),
            ),
            ("duplex".to_owned(), owned(ZValue::from(true))),
            ("buttons".to_owned(), owned(ZValue::from(buttons))),
        ])
    }

    #[test]
    fn the_documented_capabilities_decode_field_by_field() {
        let decoded = capabilities(&documented_example()).unwrap();

        assert_eq!(decoded.resolutions, [100, 200, 300, 600]);
        assert_eq!(
            decoded.color_modes,
            [ColorMode::Color, ColorMode::Gray, ColorMode::Bw]
        );
        assert_eq!(decoded.sources, [Source::Flatbed, Source::Adf]);
        assert!(decoded.duplex);
        assert_eq!(decoded.button_count(), 4);
        assert!(!decoded.buttons.label_configurable);
        assert!(decoded.extra.is_empty(), "{:?}", decoded.extra);
        assert!(decoded.supports_adf());
    }

    /// The acceptance criterion of 8.1: a key this version has no field for reaches the
    /// caller instead of being dropped on the way through.
    #[test]
    fn an_unknown_key_round_trips_instead_of_being_dropped() {
        let mut dict = documented_example();
        dict.insert(
            "max_scan_area_mm".to_owned(),
            owned(ZValue::from(vec![216u32, 356])),
        );
        dict.insert(
            "vendor".to_owned(),
            owned(ZValue::from(HashMap::from([(
                "brscan".to_owned(),
                owned(ZValue::from("brscan5")),
            )]))),
        );

        let decoded = capabilities(&dict).unwrap();
        assert_eq!(
            decoded.extra.get("max_scan_area_mm"),
            Some(&Value::Array(vec![Value::U64(216), Value::U64(356)]))
        );
        assert_eq!(
            decoded.extra.get("vendor"),
            Some(&Value::Dict(BTreeMap::from([(
                "brscan".to_owned(),
                Value::Str("brscan5".to_owned())
            )])))
        );

        // And it survives being sent back out, which is what a `button set` that only
        // meant to change one key depends on.
        let resent = to_dict(&decoded.extra);
        assert_eq!(from_dict(&resent).unwrap(), decoded.extra);
    }

    /// A scanner that reports nothing is still a scanner, not a decode failure.
    #[test]
    fn an_empty_capability_map_decodes_to_the_default() {
        assert_eq!(capabilities(&Dict::new()).unwrap(), Capabilities::default());
    }

    /// `av` and the typed arrays are the same thing once decoded — the daemon sends one
    /// spelling, a hand-built client the other, and no caller can tell them apart.
    #[test]
    fn both_spellings_of_an_array_decode_alike() {
        let mut dict = documented_example();
        dict.insert(
            "sources".to_owned(),
            owned(ZValue::from(vec![
                ZValue::from("flatbed"),
                ZValue::from("adf"),
            ])),
        );
        dict.insert(
            "resolutions".to_owned(),
            owned(ZValue::from(vec![
                ZValue::from(300u32),
                ZValue::from(600u16),
            ])),
        );

        let decoded = capabilities(&dict).unwrap();
        assert_eq!(decoded.sources, [Source::Flatbed, Source::Adf]);
        assert_eq!(decoded.resolutions, [300, 600]);
    }

    /// The closed sets of §3 stay closed: a mode nobody can act on is refused loudly
    /// rather than dropped into a list the user then picks from.
    #[test]
    fn a_member_outside_a_closed_set_is_refused_by_name() {
        let mut dict = documented_example();
        dict.insert(
            "color_modes".to_owned(),
            owned(ZValue::from(vec!["color", "cmyk"])),
        );

        let error = capabilities(&dict).expect_err("cmyk is not a colour mode of §3");
        assert_eq!(
            error,
            DecodeError::Member {
                key: "color_modes".to_owned(),
                value: "cmyk".to_owned(),
                expected: "color, gray, bw",
            }
        );
        assert!(error.to_string().contains("cmyk"), "{error}");
    }

    /// A documented key of the wrong type names itself, so the daemon's bug is legible.
    #[test]
    fn a_documented_key_of_the_wrong_type_names_itself() {
        let mut dict = documented_example();
        dict.insert("duplex".to_owned(), owned(ZValue::from("yes")));

        let error = capabilities(&dict).expect_err("duplex is a boolean");
        assert_eq!(
            error,
            DecodeError::Type {
                key: "duplex".to_owned(),
                expected: "b",
                signature: "s".to_owned(),
            }
        );

        let mut dict = documented_example();
        dict.insert(
            "buttons".to_owned(),
            owned(ZValue::from(HashMap::from([(
                "count".to_owned(),
                owned(ZValue::from(-1i32)),
            )]))),
        );
        assert!(matches!(
            capabilities(&dict),
            Err(DecodeError::Type { key, .. }) if key == "buttons.count"
        ));
    }

    /// Every shape a client sends comes back unchanged — the round-trip the CLI's
    /// `button set --option k=v` depends on.
    #[test]
    fn every_value_this_crate_sends_reads_back_unchanged() {
        let value = BTreeMap::from([
            ("flag".to_owned(), Value::Bool(true)),
            ("count".to_owned(), Value::U64(4)),
            ("offset".to_owned(), Value::I64(-1)),
            ("gamma".to_owned(), Value::F64(2.2)),
            ("folder".to_owned(), Value::Str("~/Scans".to_owned())),
            (
                "mixed".to_owned(),
                Value::Array(vec![Value::U64(300), Value::Str("a".to_owned())]),
            ),
            (
                "nested".to_owned(),
                Value::Dict(BTreeMap::from([("k".to_owned(), Value::Bool(false))])),
            ),
        ]);

        assert_eq!(from_dict(&to_dict(&value)).unwrap(), value);
    }

    /// The narrow integer signatures widen rather than being refused, which is what
    /// makes `au` readable at all.
    #[test]
    fn narrow_integers_widen_into_the_models_two() {
        for (sent, expected) in [
            (ZValue::from(7u8), Value::U64(7)),
            (ZValue::from(7u16), Value::U64(7)),
            (ZValue::from(7u32), Value::U64(7)),
            (ZValue::from(-7i16), Value::I64(-7)),
            (ZValue::from(-7i32), Value::I64(-7)),
        ] {
            assert_eq!(from_variant(&sent).unwrap(), expected);
        }
    }

    /// A `v` inside an `av` is unwrapped once, not reported as an unrepresentable type.
    #[test]
    fn a_nested_variant_is_unwrapped() {
        let nested = ZValue::from(OwnedValue::try_from(ZValue::from("inner")).unwrap());
        assert_eq!(
            from_variant(&nested).unwrap(),
            Value::Str("inner".to_owned())
        );
    }

    /// A type the model cannot hold says which one it was, so the CLI can render it as
    /// its signature (§6) instead of dropping the key.
    #[test]
    fn an_unrepresentable_type_reports_its_signature() {
        let structure = ZValue::from((1u32, "a"));
        let error = from_variant(&structure).expect_err("a struct has no model equivalent");
        assert_eq!(
            error,
            DecodeError::Signature {
                signature: "(us)".to_owned()
            }
        );
    }
}
