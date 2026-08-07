//! [`Value`]: the deliberately untyped corner of the model.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A dynamic value, for data whose shape we genuinely do not know.
///
/// Two places need one: per-button [`ProfileOptions`](super::ButtonInfo::profile_options),
/// which are profile-specific and open by design, and capability keys a backend reports
/// that this version of the daemon has no field for. Everything else in the model is a
/// typed field — the whole point of the crate is that `a{sv}` stops at the boundary.
///
/// This is not `zvariant::Value`: core must build without a bus. The daemon converts
/// between the two, which is also where the D-Bus signature is decided (`U64` becomes
/// `t`, `I64` becomes `x`, …); keeping the integer variants apart here is what lets it
/// do that without guessing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    /// `b`
    Bool(bool),
    /// `t` — tried before [`Value::I64`] when deserialising, so non-negative integers
    /// keep their unsigned type.
    U64(u64),
    /// `x`
    I64(i64),
    /// `d`
    F64(f64),
    /// `s`
    Str(String),
    /// `av`
    Array(Vec<Value>),
    /// `a{sv}`
    Dict(BTreeMap<String, Value>),
}

impl Value {
    /// The boolean inside, if this is one.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(v) => Some(*v),
            _ => None,
        }
    }

    /// The unsigned integer inside, if this is one.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::U64(v) => Some(*v),
            _ => None,
        }
    }

    /// The signed integer inside — also yields non-negative [`Value::U64`]s that fit.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::I64(v) => Some(*v),
            Self::U64(v) => i64::try_from(*v).ok(),
            _ => None,
        }
    }

    /// The string inside, if this is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(v) => Some(v),
            _ => None,
        }
    }

    /// The array inside, if this is one.
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Self::Array(v) => Some(v),
            _ => None,
        }
    }

    /// The dictionary inside, if this is one.
    pub fn as_dict(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Self::Dict(v) => Some(v),
            _ => None,
        }
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

impl From<u32> for Value {
    fn from(v: u32) -> Self {
        Self::U64(v.into())
    }
}

impl From<u64> for Value {
    fn from(v: u64) -> Self {
        Self::U64(v)
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Self::I64(v)
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Self::F64(v)
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Self::Str(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Self::Str(v.to_owned())
    }
}

impl From<Vec<Value>> for Value {
    fn from(v: Vec<Value>) -> Self {
        Self::Array(v)
    }
}

impl From<BTreeMap<String, Value>> for Value {
    fn from(v: BTreeMap<String, Value>) -> Self {
        Self::Dict(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_shape() {
        let value = Value::Dict(BTreeMap::from([
            ("flag".to_owned(), Value::Bool(true)),
            ("count".to_owned(), Value::U64(4)),
            ("offset".to_owned(), Value::I64(-1)),
            ("gamma".to_owned(), Value::F64(2.2)),
            ("folder".to_owned(), Value::Str("~/Scans".to_owned())),
            (
                "resolutions".to_owned(),
                Value::Array(vec![Value::U64(300), Value::U64(600)]),
            ),
        ]));

        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&json).unwrap(), value);
    }

    /// Untagged deserialisation picks the first variant that fits, so the ordering of
    /// the integer variants is load-bearing rather than cosmetic.
    #[test]
    fn non_negative_integers_stay_unsigned() {
        assert_eq!(
            serde_json::from_str::<Value>("300").unwrap(),
            Value::U64(300)
        );
        assert_eq!(serde_json::from_str::<Value>("-1").unwrap(), Value::I64(-1));
        assert_eq!(
            serde_json::from_str::<Value>("2.2").unwrap(),
            Value::F64(2.2)
        );
    }

    #[test]
    fn accessors_only_answer_for_their_own_variant() {
        assert_eq!(Value::from(300u32).as_u64(), Some(300));
        assert_eq!(Value::from(300u32).as_i64(), Some(300));
        assert_eq!(Value::from("300").as_u64(), None);
        assert_eq!(Value::from(true).as_bool(), Some(true));
        assert_eq!(Value::U64(u64::MAX).as_i64(), None);
    }
}
