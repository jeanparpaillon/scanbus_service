//! [`OptionsSchema`]: what a profile accepts, decoded once for every frontend.
//!
//! `Profile1.OptionsSchema` ([`scanbus-dbus-api.md`] §6) is an `a{sv}` of `a{sv}`s: one
//! entry per option key, each holding a `type`, an effective `default`, a `description`
//! and — depending on the type — a closed set of `values` or a `min`/`max` pair. The
//! daemon generates it from the same table it validates writes against, so it is the
//! contract a client builds an editor from.
//!
//! This module is where that wire shape stops. A GUI that walked the raw maps itself
//! would be a second decoder of the same property, free to disagree with the CLI's about
//! what a `u32` bound or a missing `values` means — which is the duplication this crate
//! exists to remove.
//!
//! # An entry this version cannot type is kept, not refused
//!
//! §6 fixes four type names today and says clients must ignore fields they do not
//! recognise, because the option set is deliberately open: `ocr` will bring a language
//! list, `email` template strings. So nothing inside an entry is fatal here. An unknown
//! `type`, a `type` that is not even a string, a `values` that is not an array of strings
//! — each yields [`OptionType::Unknown`] with every field preserved in
//! [`OptionSchema::extra`], and the consumer renders the read-only row §6 asks for. The
//! alternative — one odd entry failing the decode — would turn an additive daemon change
//! into a blank Profiles page, which is exactly the breakage the property was added to
//! avoid.
//!
//! Degrading to [`OptionType::Unknown`] rather than to something weaker is deliberate: a
//! `string` whose `values` did not decode must not become a free-text entry, or the
//! client would offer values the daemon then rejects. "We cannot describe a trustworthy
//! widget for this" is the honest reading, and it is one the GUI can act on.
//!
//! Only a shape the model itself cannot hold is an error — an entry that is not a map, or
//! a value [`crate::convert::from_variant`] refuses, such as a struct.
//!
//! # Integers are `i128` here
//!
//! §6 forbids assuming an integer width: a bound may arrive as any D-Bus integer type,
//! and [`Value`] has already widened it to `t` or `x` by the time it reaches this module.
//! `i128` is the one Rust integer that holds both without a fallible conversion, so a
//! consumer reading [`OptionType::Integer`] never has to decide what to do with a `u64`
//! that does not fit an `i64`.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md

use std::collections::BTreeMap;

use scanbus_core::{ProfileKind, Value};
use zbus::Connection;

use crate::convert::{self, DecodeError, Dict};
use crate::proxy::Profile1Proxy;

/// The `type` of a string option, restricted to `values` when that list is non-empty.
pub const TYPE_STRING: &str = "string";
/// The `type` of an integer option, bounded by `min`/`max` when they are published.
pub const TYPE_INTEGER: &str = "integer";
/// The `type` of a boolean option.
pub const TYPE_BOOLEAN: &str = "boolean";
/// The `type` of a string that carries a local filesystem path.
pub const TYPE_PATH: &str = "path";

/// What one option accepts — §6's widget-level vocabulary, not a D-Bus signature.
///
/// [`OptionType::Path`] is a string like [`OptionType::Str`] is; it is a variant of its
/// own only so a client can offer a folder chooser instead of a text entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionType {
    /// A string. `values` is every accepted spelling, and empty when any string is
    /// accepted.
    ///
    /// The list may contain aliases of one another — `image.format` publishes `jpeg`,
    /// `jpg` and `png`, where the first two produce identical output (§6). Nothing marks
    /// them, so a picker may collapse them but must accept any spelling coming back in
    /// `Options`.
    Str {
        /// Every value the daemon accepts, in the order it published them.
        values: Vec<String>,
    },

    /// An integer, within the inclusive bounds the daemon published.
    ///
    /// §6 has the daemon emit `min` and `max` together, but they are separately optional
    /// here: a half-bounded range is representable on the wire, and dropping the half
    /// that did arrive would be inventing a constraint.
    Integer {
        /// Inclusive lower bound.
        min: Option<i128>,
        /// Inclusive upper bound.
        max: Option<i128>,
    },

    /// A boolean.
    Boolean,

    /// A string carrying a local filesystem path.
    Path,

    /// A `type` this version has no widget for, or an entry whose fields did not decode.
    ///
    /// Everything the entry carried is still in [`OptionSchema::extra`], so a consumer
    /// can show it, log it, or grow support for it later. See the module documentation
    /// for why this is not an error.
    Unknown {
        /// What the daemon called it — `""` when the entry published no usable `type`.
        type_name: String,
    },
}

impl OptionType {
    /// The `type` string this came from, as §6 spells it.
    pub fn type_name(&self) -> &str {
        match self {
            Self::Str { .. } => TYPE_STRING,
            Self::Integer { .. } => TYPE_INTEGER,
            Self::Boolean => TYPE_BOOLEAN,
            Self::Path => TYPE_PATH,
            Self::Unknown { type_name } => type_name,
        }
    }
}

/// One option a profile accepts: its type, its effective default and its description.
#[derive(Debug, Clone, PartialEq)]
pub struct OptionSchema {
    /// The key in `Options`, `ProfileOptions` and `OptionsSchema` alike.
    pub key: String,

    /// What a value has to be — and what widget describes it.
    pub value: OptionType,

    /// The value the daemon *will use* when the key is absent, not what the profile
    /// store happens to hold.
    ///
    /// This is what a client displays for an unset option, as a value inherited from the
    /// daemon rather than one the user chose — `output_folder` is normally unset and its
    /// default is the directory the daemon resolves from the XDG user dirs (§6).
    ///
    /// [`None`] means the entry published none. §6 says it always does, so this is a
    /// daemon newer or odder than this client; the option is still listed rather than
    /// dropped.
    pub default: Option<Value>,

    /// One short untranslated English line — a fallback label or tooltip. `""` when the
    /// entry published none.
    pub description: String,

    /// Every field of the entry this version has no field for, and the ones that did not
    /// decode into [`OptionSchema::value`].
    ///
    /// An [`OptionType::Unknown`] keeps its `values`, `min` and `max` here, which is what
    /// makes an unknown type inspectable rather than merely survivable.
    pub extra: BTreeMap<String, Value>,
}

impl OptionSchema {
    /// The default as a string, for [`OptionType::Str`] and [`OptionType::Path`].
    pub fn default_str(&self) -> Option<&str> {
        self.default.as_ref()?.as_str()
    }

    /// The default as an integer, whichever way round the daemon signed it.
    pub fn default_integer(&self) -> Option<i128> {
        integer(self.default.as_ref()?)
    }

    /// The default as a boolean.
    pub fn default_bool(&self) -> Option<bool> {
        self.default.as_ref()?.as_bool()
    }

    /// §6's display rule, in one call: the stored value when it is set, the effective
    /// default otherwise.
    ///
    /// `stored` is what `Options` — or a `Button1.ProfileOptions` override — holds for
    /// this key. A caller walking the two-level chain of §6 passes the button's value
    /// first and falls back to the profile's itself:
    ///
    /// ```text
    /// schema.effective(button.get(key).filter(|v| !is_unset(v)).or(profile.get(key)))
    /// ```
    pub fn effective<'a>(&'a self, stored: Option<&'a Value>) -> Option<&'a Value> {
        match stored {
            Some(value) if !is_unset(value) => Some(value),
            _ => self.default.as_ref(),
        }
    }
}

/// Whether §6 treats this value as "not set".
///
/// An `output_folder` that is present but empty or whitespace-only is handled by the
/// daemon exactly like an absent one, so a client that showed it as a chosen value would
/// be reporting a destination no scan will use.
pub fn is_unset(value: &Value) -> bool {
    match value {
        Value::Str(text) => text.trim().is_empty(),
        _ => false,
    }
}

/// Every option one profile accepts, keyed by option name.
///
/// Ordered by key: `a{sv}` is unordered on the wire and reaches this crate as a
/// [`std::collections::HashMap`], so declaration order is already lost before any
/// decoding happens. A consumer that wants a different order in its editor has to impose
/// one itself.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OptionsSchema {
    entries: BTreeMap<String, OptionSchema>,
}

impl OptionsSchema {
    /// Reads this profile's `OptionsSchema` off its object.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Bus`] when the profile has no object — `email` and `ocr` are valid
    /// [`ProfileKind`]s with nothing exported for them (§6) — and
    /// [`crate::Error::Decode`] for a property this version cannot make sense of.
    pub async fn fetch(connection: &Connection, kind: ProfileKind) -> crate::Result<Self> {
        let proxy = Profile1Proxy::for_profile(connection, kind).await?;
        Ok(Self::decode(&proxy.options_schema().await?)?)
    }

    /// Decodes the property's raw `a{sv}`.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Type`] for an entry that is not a map — the one thing §6's shape
    /// cannot survive — and whatever [`convert::from_variant`] refuses inside one.
    pub fn decode(dict: &Dict) -> Result<Self, DecodeError> {
        let decoded = convert::from_dict(dict)?;
        let mut entries = BTreeMap::new();

        for (key, value) in decoded {
            let Value::Dict(fields) = value else {
                return Err(DecodeError::Type {
                    key: format!("OptionsSchema.{key}"),
                    expected: "a{sv}",
                    signature: signature(&value).to_owned(),
                });
            };
            entries.insert(key.clone(), entry(key, fields));
        }

        Ok(Self { entries })
    }

    /// The entry for one option, or [`None`] when the profile does not accept that key.
    pub fn get(&self, key: &str) -> Option<&OptionSchema> {
        self.entries.get(key)
    }

    /// Every entry, ordered by key — what an editor walks to build its rows.
    pub fn iter(&self) -> impl Iterator<Item = &OptionSchema> {
        self.entries.values()
    }

    /// How many options this profile accepts.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the profile published no options at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// §6's display rule for one key: see [`OptionSchema::effective`].
    ///
    /// A key with no entry still yields its stored value — an option the daemon dropped
    /// from the schema is one a client should stop offering, not one whose current value
    /// it should hide.
    pub fn effective<'a>(
        &'a self,
        key: &str,
        options: &'a BTreeMap<String, Value>,
    ) -> Option<&'a Value> {
        match self.entries.get(key) {
            Some(entry) => entry.effective(options.get(key)),
            None => options.get(key),
        }
    }
}

impl<'a> IntoIterator for &'a OptionsSchema {
    type Item = &'a OptionSchema;
    type IntoIter = std::collections::btree_map::Values<'a, String, OptionSchema>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.values()
    }
}

/// One entry's fields, typed as far as they go — see the module documentation for what
/// "as far as they go" means.
fn entry(key: String, mut fields: BTreeMap<String, Value>) -> OptionSchema {
    let type_name = fields
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    let value = match type_name.as_str() {
        TYPE_STRING => match fields.remove("values") {
            None => OptionType::Str { values: Vec::new() },
            Some(values) => match strings(&values) {
                Some(values) => OptionType::Str { values },
                None => {
                    // Not a list this client can offer: put it back, and let the entry be
                    // the read-only row rather than a text field the daemon will refuse.
                    fields.insert("values".to_owned(), values);
                    unknown(&type_name)
                }
            },
        },
        TYPE_INTEGER => {
            let min = fields.remove("min");
            let max = fields.remove("max");
            match (bound(min.as_ref()), bound(max.as_ref())) {
                (Ok(min), Ok(max)) => OptionType::Integer { min, max },
                _ => {
                    if let Some(min) = min {
                        fields.insert("min".to_owned(), min);
                    }
                    if let Some(max) = max {
                        fields.insert("max".to_owned(), max);
                    }
                    unknown(&type_name)
                }
            }
        }
        TYPE_BOOLEAN => OptionType::Boolean,
        TYPE_PATH => OptionType::Path,
        _ => unknown(&type_name),
    };

    fields.remove("type");
    let default = fields.remove("default");
    let description = fields
        .remove("description")
        .as_ref()
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    OptionSchema {
        key,
        value,
        default,
        description,
        extra: fields,
    }
}

fn unknown(type_name: &str) -> OptionType {
    OptionType::Unknown {
        type_name: type_name.to_owned(),
    }
}

/// The `as` of `values`, from either the typed array or the `av` a hand-built client
/// sends — [`convert::from_variant`] has already flattened both into the same
/// [`Value::Array`].
fn strings(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect()
}

/// A `min`/`max`, absent or integral. [`Err`] is "present and not an integer", which is
/// what makes the whole entry [`OptionType::Unknown`].
fn bound(value: Option<&Value>) -> Result<Option<i128>, ()> {
    match value {
        None => Ok(None),
        Some(value) => integer(value).map(Some).ok_or(()),
    }
}

/// The integer a value carries, from either of the model's two integer variants.
fn integer(value: &Value) -> Option<i128> {
    match value {
        Value::U64(number) => Some(i128::from(*number)),
        Value::I64(number) => Some(i128::from(*number)),
        _ => None,
    }
}

/// The model's vocabulary for what arrived, for a [`DecodeError::Type`].
fn signature(value: &Value) -> &'static str {
    match value {
        Value::Bool(_) => "b",
        Value::U64(_) => "t",
        Value::I64(_) => "x",
        Value::F64(_) => "d",
        Value::Str(_) => "s",
        Value::Array(_) => "av",
        Value::Dict(_) => "a{sv}",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use zbus::zvariant::{OwnedValue, Value as ZValue};

    use super::*;

    fn owned(value: ZValue<'_>) -> OwnedValue {
        OwnedValue::try_from(value).unwrap()
    }

    fn schema_entry(fields: Vec<(&str, OwnedValue)>) -> OwnedValue {
        owned(ZValue::from(
            fields
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect::<HashMap<String, OwnedValue>>(),
        ))
    }

    /// The `image` schema of §6, in the signatures the daemon renders it with: `as` for
    /// `values`, `t` for the bounds, `s` for the rest.
    fn image() -> Dict {
        Dict::from([
            (
                "format".to_owned(),
                schema_entry(vec![
                    ("type", owned(ZValue::from("string"))),
                    ("default", owned(ZValue::from("jpeg"))),
                    ("values", owned(ZValue::from(vec!["jpeg", "jpg", "png"]))),
                    (
                        "description",
                        owned(ZValue::from("Encoding of the written page files")),
                    ),
                ]),
            ),
            (
                "quality".to_owned(),
                schema_entry(vec![
                    ("type", owned(ZValue::from("integer"))),
                    ("default", owned(ZValue::from(90u64))),
                    ("min", owned(ZValue::from(1u64))),
                    ("max", owned(ZValue::from(100u64))),
                    (
                        "description",
                        owned(ZValue::from("JPEG quality; ignored when format is png")),
                    ),
                ]),
            ),
            (
                "multi_page".to_owned(),
                schema_entry(vec![
                    ("type", owned(ZValue::from("boolean"))),
                    ("default", owned(ZValue::from(true))),
                    ("description", owned(ZValue::from("One file for the batch"))),
                ]),
            ),
            (
                "output_folder".to_owned(),
                schema_entry(vec![
                    ("type", owned(ZValue::from("path"))),
                    (
                        "default",
                        owned(ZValue::from("/home/user/Pictures/scanbus/image")),
                    ),
                    (
                        "description",
                        owned(ZValue::from("Directory the pages are written to")),
                    ),
                ]),
            ),
        ])
    }

    /// Acceptance: the documented `image` entries reach a consumer typed, with no
    /// `zvariant` in sight.
    #[test]
    fn the_documented_image_schema_decodes_entry_by_entry() {
        let schema = OptionsSchema::decode(&image()).unwrap();
        assert_eq!(schema.len(), 4);

        let format = schema.get("format").unwrap();
        assert_eq!(
            format.value,
            OptionType::Str {
                values: vec!["jpeg".to_owned(), "jpg".to_owned(), "png".to_owned()],
            }
        );
        assert_eq!(format.default_str(), Some("jpeg"));
        assert_eq!(format.description, "Encoding of the written page files");
        assert!(format.extra.is_empty(), "{:?}", format.extra);

        let quality = schema.get("quality").unwrap();
        assert_eq!(
            quality.value,
            OptionType::Integer {
                min: Some(1),
                max: Some(100),
            }
        );
        assert_eq!(quality.default_integer(), Some(90));

        let multi_page = schema.get("multi_page").unwrap();
        assert_eq!(multi_page.value, OptionType::Boolean);
        assert_eq!(multi_page.default_bool(), Some(true));

        let folder = schema.get("output_folder").unwrap();
        assert_eq!(folder.value, OptionType::Path);
        assert_eq!(
            folder.default_str(),
            Some("/home/user/Pictures/scanbus/image"),
            "the resolved directory, which is the whole point of publishing a default"
        );

        // And an editor walks them in one pass, without knowing any key by name.
        assert_eq!(
            schema
                .iter()
                .map(|entry| (entry.key.as_str(), entry.value.type_name()))
                .collect::<Vec<_>>(),
            [
                ("format", "string"),
                ("multi_page", "boolean"),
                ("output_folder", "path"),
                ("quality", "integer"),
            ]
        );
    }

    /// Acceptance: a `type` this version has never heard of stays inspectable — the
    /// entry is listed, named, and keeps every field it arrived with.
    #[test]
    fn an_unknown_type_survives_with_its_fields() {
        let mut dict = image();
        dict.insert(
            "languages".to_owned(),
            schema_entry(vec![
                ("type", owned(ZValue::from("string-list"))),
                ("default", owned(ZValue::from(vec!["eng"]))),
                ("values", owned(ZValue::from(vec!["eng", "fra", "deu"]))),
                ("max_selected", owned(ZValue::from(3u32))),
                ("description", owned(ZValue::from("OCR languages"))),
            ]),
        );

        let schema = OptionsSchema::decode(&dict).unwrap();
        let languages = schema.get("languages").expect("the entry must be listed");

        assert_eq!(
            languages.value,
            OptionType::Unknown {
                type_name: "string-list".to_owned()
            }
        );
        assert_eq!(languages.value.type_name(), "string-list");
        assert_eq!(languages.description, "OCR languages");
        assert_eq!(
            languages.default,
            Some(Value::Array(vec![Value::Str("eng".to_owned())])),
            "a default this client cannot type is still a default it can show"
        );
        assert_eq!(
            languages.extra.get("values"),
            Some(&Value::Array(vec![
                Value::Str("eng".to_owned()),
                Value::Str("fra".to_owned()),
                Value::Str("deu".to_owned()),
            ]))
        );
        assert_eq!(
            languages.extra.get("max_selected"),
            Some(&Value::U64(3)),
            "a field this version has no meaning for is kept, not dropped"
        );

        // And the entries this client does understand are unaffected.
        assert_eq!(schema.get("format").unwrap().value.type_name(), "string");
    }

    /// A field a known type needs, malformed: the row degrades to read-only rather than
    /// to a text entry offering values the daemon would refuse.
    #[test]
    fn a_type_whose_constraints_did_not_decode_degrades_to_unknown() {
        let mut dict = image();
        dict.insert(
            "format".to_owned(),
            schema_entry(vec![
                ("type", owned(ZValue::from("string"))),
                ("default", owned(ZValue::from("jpeg"))),
                ("values", owned(ZValue::from(vec![1u32, 2]))),
            ]),
        );
        dict.insert(
            "quality".to_owned(),
            schema_entry(vec![
                ("type", owned(ZValue::from("integer"))),
                ("default", owned(ZValue::from(90u64))),
                ("min", owned(ZValue::from("one"))),
                ("max", owned(ZValue::from(100u64))),
            ]),
        );

        let schema = OptionsSchema::decode(&dict).unwrap();

        let format = schema.get("format").unwrap();
        assert_eq!(
            format.value,
            OptionType::Unknown {
                type_name: "string".to_owned()
            }
        );
        assert_eq!(
            format.extra.get("values"),
            Some(&Value::Array(vec![Value::U64(1), Value::U64(2)])),
            "the list that did not decode is kept for whoever debugs the daemon"
        );

        let quality = schema.get("quality").unwrap();
        assert!(matches!(quality.value, OptionType::Unknown { .. }));
        assert_eq!(
            quality.extra.get("min"),
            Some(&Value::Str("one".to_owned()))
        );
        assert_eq!(quality.extra.get("max"), Some(&Value::U64(100)));
    }

    /// §6 says `type` and `default` are always there. A daemon that omits them is odd,
    /// not a reason to lose the whole profile page.
    #[test]
    fn an_entry_missing_its_type_or_default_is_still_an_entry() {
        let dict = Dict::from([
            (
                "mystery".to_owned(),
                schema_entry(vec![("description", owned(ZValue::from("?")))]),
            ),
            (
                "subject".to_owned(),
                schema_entry(vec![("type", owned(ZValue::from("string")))]),
            ),
        ]);

        let schema = OptionsSchema::decode(&dict).unwrap();

        let mystery = schema.get("mystery").unwrap();
        assert_eq!(
            mystery.value,
            OptionType::Unknown {
                type_name: String::new()
            }
        );
        assert_eq!(mystery.default, None);

        let subject = schema.get("subject").unwrap();
        assert_eq!(subject.value, OptionType::Str { values: Vec::new() });
        assert_eq!(subject.default, None);
        assert_eq!(subject.description, "", "absent is not an error either");
    }

    /// The one shape §6 cannot survive: an entry that is not a map at all.
    #[test]
    fn an_entry_that_is_not_a_map_names_itself() {
        let dict = Dict::from([("format".to_owned(), owned(ZValue::from("jpeg")))]);

        assert_eq!(
            OptionsSchema::decode(&dict).unwrap_err(),
            DecodeError::Type {
                key: "OptionsSchema.format".to_owned(),
                expected: "a{sv}",
                signature: "s".to_owned(),
            }
        );
    }

    /// §6: no assuming an integer width. Every D-Bus integer signature a bound may
    /// arrive with reads back as the same number.
    #[test]
    fn bounds_are_read_at_whatever_width_they_arrive_in() {
        for (min, max, expected_max) in [
            (ZValue::from(1u8), ZValue::from(100u8), 100i128),
            (ZValue::from(1u16), ZValue::from(600u16), 600),
            (ZValue::from(1i32), ZValue::from(4096i32), 4096),
            (
                ZValue::from(1i64),
                ZValue::from(i64::MAX),
                i128::from(i64::MAX),
            ),
            (
                ZValue::from(1u64),
                ZValue::from(u64::MAX),
                i128::from(u64::MAX),
            ),
        ] {
            let dict = Dict::from([(
                "quality".to_owned(),
                schema_entry(vec![
                    ("type", owned(ZValue::from("integer"))),
                    ("default", owned(ZValue::from(90u32))),
                    ("min", owned(min)),
                    ("max", owned(max)),
                ]),
            )]);

            let schema = OptionsSchema::decode(&dict).unwrap();
            let quality = schema.get("quality").unwrap();
            assert_eq!(
                quality.value,
                OptionType::Integer {
                    min: Some(1),
                    max: Some(expected_max),
                }
            );
            assert_eq!(quality.default_integer(), Some(90));
        }
    }

    /// The display rule of §6, which is the reason `default` is the *effective* one:
    /// stored when set, the daemon's own value otherwise — including for the empty
    /// string the daemon treats as unset.
    #[test]
    fn the_effective_value_is_the_stored_one_or_the_default() {
        let schema = OptionsSchema::decode(&image()).unwrap();
        let options = BTreeMap::from([
            ("format".to_owned(), Value::Str("png".to_owned())),
            ("output_folder".to_owned(), Value::Str("   ".to_owned())),
            ("resolution".to_owned(), Value::U64(600)),
        ]);

        assert_eq!(
            schema.effective("format", &options),
            Some(&Value::Str("png".to_owned())),
            "a value the user chose"
        );
        assert_eq!(
            schema.effective("quality", &options),
            Some(&Value::U64(90)),
            "a key the store does not hold falls back to the effective default"
        );
        assert_eq!(
            schema.effective("output_folder", &options),
            Some(&Value::Str("/home/user/Pictures/scanbus/image".to_owned())),
            "whitespace is unset, and showing it would name a directory nothing writes to"
        );
        assert_eq!(
            schema.effective("resolution", &options),
            Some(&Value::U64(600)),
            "a stored key the schema no longer declares is still what is set"
        );
        assert_eq!(schema.effective("nothing", &options), None);
    }

    /// A profile that publishes nothing decodes to an empty schema, not to a failure.
    #[test]
    fn an_empty_schema_is_not_an_error() {
        let schema = OptionsSchema::decode(&Dict::new()).unwrap();
        assert!(schema.is_empty());
        assert_eq!(schema.iter().count(), 0);
    }
}
