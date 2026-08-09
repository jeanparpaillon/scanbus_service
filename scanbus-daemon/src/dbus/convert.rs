//! The model, rendered onto the wire.
//!
//! `scanbus-core` is typed where D-Bus is not, and says so: `Capabilities` is a struct
//! with fields rather than the `a{sv}` of [`scanbus-dbus-api.md`] §3, and
//! [`scanbus_core::Value`] is deliberately not `zvariant::Value` because core must build
//! without a bus. This module is the one place the two meet, so that the D-Bus
//! signatures of the model are decided once instead of at each property getter.
//!
//! The direction is out only, for now. Reading an `a{sv}` back — `ProfileOptions` writes
//! (2.5), `Pair()` options (2.3) — needs the inverse, and lands with the first interface
//! that accepts one.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md

use std::collections::HashMap;

use scanbus_core::{Capabilities, Value};
use zbus::zvariant::{OwnedValue, Value as ZValue};

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

/// Renders [`Capabilities`] as the `Capabilities` property of §3.
///
/// The documented keys are always present, including the empty and `false` ones: a
/// client branching on `duplex` should read `false`, not have to tell "absent" from "not
/// supported". [`Capabilities::extra`] — the keys a backend reported that this version
/// has no field for — is merged in underneath, so a known key always wins over a
/// backend's homonym rather than the merge order deciding it.
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

    rendered
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
