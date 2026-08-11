use std::collections::BTreeMap;
use std::path::PathBuf;

use scanbus_client::{Error as ClientError, ScanbusError};
use scanbus_core::Value;

use crate::error::{Error, Result};

pub(crate) fn parse_options(
    options: &[String],
    option_json: &[String],
) -> Result<BTreeMap<String, Value>> {
    let mut parsed = BTreeMap::new();

    for option in options {
        let (key, raw) = split_option(option, "--option")?;
        parsed.insert(key.to_owned(), expand_value(parse_scalar(raw)));
    }

    for option in option_json {
        let (key, raw) = split_option(option, "--option-json")?;
        let value = serde_json::from_str::<Value>(raw).map_err(|error| {
            Error::call(
                "parsing --option-json",
                ClientError::Call(ScanbusError::Other {
                    name: "org.scanbus.internal.InvalidOptionJson".to_owned(),
                    message: format!("option {option:?} is not valid JSON: {error}"),
                }),
            )
        })?;
        parsed.insert(key.to_owned(), expand_value(value));
    }

    Ok(parsed)
}

pub(crate) fn render_options(options: &BTreeMap<String, Value>) -> String {
    if options.is_empty() {
        return String::new();
    }

    options
        .iter()
        .map(|(key, value)| format!("{key}={}", render_value(value)))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn render_value(value: &Value) -> String {
    match value {
        Value::Bool(value) => value.to_string(),
        Value::U64(value) => value.to_string(),
        Value::I64(value) => value.to_string(),
        Value::F64(value) => value.to_string(),
        Value::Str(value) => value.clone(),
        Value::Array(_) | Value::Dict(_) => {
            serde_json::to_string(value).expect("scanbus_core::Value always serializes to JSON")
        }
    }
}

fn split_option<'a>(option: &'a str, flag: &str) -> Result<(&'a str, &'a str)> {
    option.split_once('=').ok_or_else(|| {
        Error::call(
            format!("parsing {flag}"),
            ClientError::Call(ScanbusError::Other {
                name: "org.scanbus.internal.InvalidOption".to_owned(),
                message: format!("option {option:?} must be written as K=V"),
            }),
        )
    })
}

fn parse_scalar(raw: &str) -> Value {
    match raw {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => match raw.parse::<i64>() {
            Ok(value) if value >= 0 => Value::U64(value as u64),
            Ok(value) => Value::I64(value),
            Err(_) => Value::Str(raw.to_owned()),
        },
    }
}

fn expand_value(value: Value) -> Value {
    match value {
        Value::Str(text) => Value::Str(expand_tilde(&text)),
        other => other,
    }
}

fn expand_tilde(value: &str) -> String {
    if value == "~" {
        return home_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| value.to_owned());
    }

    if let Some(rest) = value.strip_prefix("~/") {
        return home_dir()
            .map(|path| path.join(rest).display().to_string())
            .unwrap_or_else(|| value.to_owned());
    }

    value.to_owned()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
