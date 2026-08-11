//! `scanbus button …` — the physical menu, and what this host has assigned to it ([8.9]).
//!
//! [8.9]: https://github.com/jeanparpaillon/scanbus_service/issues/37

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;

use scanbus_client::convert;
use scanbus_client::proxy::{Button1Proxy, Manager1Proxy};
use scanbus_client::{Button, Connection, Error as ClientError, Objects, ScanbusError, Scanner};
use scanbus_core::Value;

use crate::cli::ScannerArg;
use crate::context::Context;
use crate::error::{Error, Result};
use crate::output::{self, Format};

pub async fn list(context: &Context, scanner: &ScannerArg) -> Result<u8> {
    let connection = context.connect().await?;
    let objects = context
        .within("listing the daemon's objects", Objects::fetch(&connection))
        .await?;
    let found = resolve_scanner(&objects, scanner)?;

    let mut buttons: Vec<Button> = objects
        .buttons()
        .iter()
        .filter(|button| button.scanner == found.id)
        .cloned()
        .collect();
    buttons.sort_by_key(|button| button.index);

    let mut views = Vec::new();
    for button in &buttons {
        match read_button(context, &connection, button).await {
            Ok(view) => views.push(view),
            Err(error) if is_gone(&error) => {}
            Err(error) => return Err(error),
        }
    }

    report_list(context, found, &views)?;
    Ok(0)
}

pub async fn set(
    context: &Context,
    scanner: &ScannerArg,
    selector: &str,
    profile: &Option<String>,
    label: &Option<String>,
    options: &[String],
    option_json: &[String],
) -> Result<u8> {
    let connection = context.connect().await?;
    let objects = context
        .within("listing the daemon's objects", Objects::fetch(&connection))
        .await?;
    let found = resolve_scanner(&objects, scanner)?;
    let button = resolve_button(&objects, found, selector)?;
    let proxy = button_proxy(context, &connection, &button).await?;

    let target_profile = validate_profile(context, &connection, profile.as_deref()).await?;
    let target_options = parse_options(options, option_json)?;
    let current = read_button(context, &connection, &button).await?;

    if let Some(label) = label {
        refuse_fixed_label(&current, label)?;
    }

    let write_result = write_changes(
        context,
        &proxy,
        &button,
        label.as_deref(),
        target_profile.as_deref(),
        if options.is_empty() && option_json.is_empty() {
            None
        } else {
            Some(&target_options)
        },
    )
    .await;

    let final_view = read_button(context, &connection, &button).await?;
    report_change(context, &final_view)?;

    write_result?;
    Ok(0)
}

pub async fn clear(context: &Context, scanner: &ScannerArg, selector: &str) -> Result<u8> {
    let connection = context.connect().await?;
    let objects = context
        .within("listing the daemon's objects", Objects::fetch(&connection))
        .await?;
    let found = resolve_scanner(&objects, scanner)?;
    let button = resolve_button(&objects, found, selector)?;
    let proxy = button_proxy(context, &connection, &button).await?;

    let write_result = write_changes(
        context,
        &proxy,
        &button,
        None,
        Some(""),
        Some(&BTreeMap::new()),
    )
    .await;

    let final_view = read_button(context, &connection, &button).await?;
    report_change(context, &final_view)?;

    write_result?;
    Ok(0)
}

#[derive(Debug, Clone, PartialEq)]
struct ButtonView {
    index: u32,
    device_label: String,
    label_configurable: bool,
    label: String,
    profile: String,
    profile_options: BTreeMap<String, Value>,
}

async fn read_button(
    context: &Context,
    connection: &Connection,
    button: &Button,
) -> Result<ButtonView> {
    let proxy = button_proxy(context, connection, button).await?;
    let what = format!("reading button {} of {}", button.index, button.scanner);

    let index = context
        .within(what.clone(), async {
            proxy.index().await.map_err(ClientError::from)
        })
        .await?;
    let device_label = context
        .within(what.clone(), async {
            proxy.device_label().await.map_err(ClientError::from)
        })
        .await?;
    let label_configurable = context
        .within(what.clone(), async {
            proxy.label_configurable().await.map_err(ClientError::from)
        })
        .await?;
    let label = context
        .within(what.clone(), async {
            proxy.label().await.map_err(ClientError::from)
        })
        .await?;
    let profile = context
        .within(what.clone(), async {
            proxy.profile().await.map_err(ClientError::from)
        })
        .await?;
    let profile_options = context
        .within(what.clone(), async {
            proxy.profile_options().await.map_err(ClientError::from)
        })
        .await?;

    Ok(ButtonView {
        index,
        device_label,
        label_configurable,
        label,
        profile,
        profile_options: convert::from_dict(&profile_options)
            .map_err(ClientError::from)
            .map_err(|error| Error::call(what, error))?,
    })
}

async fn button_proxy(
    context: &Context,
    connection: &Connection,
    button: &Button,
) -> Result<Button1Proxy<'static>> {
    context
        .within(
            format!("resolving button {} of {}", button.index, button.scanner),
            async {
                Button1Proxy::builder(connection)
                    .path(button.path())
                    .map_err(ClientError::from)?
                    .cache_properties(zbus::proxy::CacheProperties::No)
                    .build()
                    .await
                    .map_err(ClientError::from)
            },
        )
        .await
}

fn resolve_scanner<'a>(objects: &'a Objects, scanner: &ScannerArg) -> Result<&'a Scanner> {
    objects
        .scanner(&scanner.scanner, scanner.matching())
        .map_err(|error| Error::call("finding the scanner", error.into()))
}

fn resolve_button(objects: &Objects, scanner: &Scanner, selector: &str) -> Result<Button> {
    objects
        .button(&scanner.id, selector)
        .cloned()
        .map_err(|error| Error::call("finding the button", error.into()))
}

async fn validate_profile(
    context: &Context,
    connection: &Connection,
    profile: Option<&str>,
) -> Result<Option<String>> {
    let Some(profile_name) = profile else {
        return Ok(None);
    };

    let available = context
        .within("reading the profile types", async {
            let manager = Manager1Proxy::new(connection).await?;
            manager.get_profile_types().await.map_err(ClientError::from)
        })
        .await?;

    if available.iter().any(|candidate| candidate == profile_name) {
        return Ok(Some(profile_name.to_owned()));
    }

    Err(Error::call(
        format!("validating profile {profile_name:?}"),
        ClientError::Call(ScanbusError::UnsupportedProfile(format!(
            "profile {profile_name:?} is not available here; choose one of {}",
            available.join(", ")
        ))),
    ))
}

fn parse_options(options: &[String], option_json: &[String]) -> Result<BTreeMap<String, Value>> {
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

fn refuse_fixed_label(button: &ButtonView, label: &str) -> Result<()> {
    if button.label_configurable {
        return Ok(());
    }

    Err(Error::call(
        format!("setting label on button {}", button.index),
        ClientError::Call(ScanbusError::Other {
            name: "org.freedesktop.DBus.Error.PropertyReadOnly".to_owned(),
            message: format!(
                "button {} has a fixed device label ({:?}); this device exposes \
                 LabelConfigurable=false, so use --profile instead of --label {:?}",
                button.index, button.device_label, label
            ),
        }),
    ))
}

async fn write_changes(
    context: &Context,
    proxy: &Button1Proxy<'_>,
    button: &Button,
    label: Option<&str>,
    profile: Option<&str>,
    options: Option<&BTreeMap<String, Value>>,
) -> Result<()> {
    if let Some(label) = label {
        context
            .within(
                format!(
                    "setting label on button {} of {}",
                    button.index, button.scanner
                ),
                async { proxy.set_label(label).await.map_err(ClientError::from) },
            )
            .await?;
    }

    if let Some(profile) = profile {
        context
            .within(
                format!(
                    "setting profile on button {} of {}",
                    button.index, button.scanner
                ),
                async { proxy.set_profile(profile).await.map_err(ClientError::from) },
            )
            .await?;
    }

    if let Some(options) = options {
        context
            .within(
                format!(
                    "setting profile options on button {} of {}",
                    button.index, button.scanner
                ),
                async {
                    proxy
                        .set_profile_options(convert::to_dict(options))
                        .await
                        .map_err(ClientError::from)
                },
            )
            .await?;
    }

    Ok(())
}

fn report_list(context: &Context, scanner: &Scanner, buttons: &[ButtonView]) -> Result<()> {
    match context.format {
        Format::Json => {
            let mut stdout = std::io::stdout().lock();
            let values = buttons.iter().map(json).collect();
            output::json(&mut stdout, &serde_json::Value::Array(values))
        }
        Format::Human => {
            if buttons.is_empty() {
                eprintln!("scanbus: {} exports no buttons right now", scanner.id);
                return Ok(());
            }

            let mut stdout = std::io::stdout().lock();
            let rows = buttons.iter().map(row).collect::<Vec<_>>();
            output::table(
                &mut stdout,
                context.style,
                &[
                    "IDX",
                    "DEVICE LABEL",
                    "CONFIGURABLE",
                    "LABEL",
                    "PROFILE",
                    "OPTIONS",
                ],
                &rows,
            )
        }
    }
}

fn report_change(context: &Context, button: &ButtonView) -> Result<()> {
    let mut stdout = std::io::stdout().lock();

    match context.format {
        Format::Json => {
            let mut value = json(button);
            value["LabelProfileDivergesFromDeviceLabel"] =
                serde_json::Value::Bool(label_profile_diverges(button));
            output::json(&mut stdout, &value)?;
        }
        Format::Human => {
            output::fields(
                &mut stdout,
                context.style,
                &[
                    ("index", button.index.to_string()),
                    ("device label", button.device_label.clone()),
                    ("configurable", yes_no(button.label_configurable).to_owned()),
                    ("label", button.label.clone()),
                    ("profile", button.profile.clone()),
                    ("options", render_options(&button.profile_options)),
                ],
            )?;

            if label_profile_diverges(button) {
                writeln!(
                    stdout,
                    "note  device label {:?} still points at {}, while this host will run {}",
                    button.device_label,
                    profile_hint(&button.device_label).unwrap_or("that firmware preset"),
                    button.profile
                )
                .map_err(Error::write)?;
            }
        }
    }

    Ok(())
}

fn row(button: &ButtonView) -> Vec<String> {
    vec![
        button.index.to_string(),
        button.device_label.clone(),
        yes_no(button.label_configurable).to_owned(),
        button.label.clone(),
        button.profile.clone(),
        render_options(&button.profile_options),
    ]
}

fn json(button: &ButtonView) -> serde_json::Value {
    serde_json::json!({
        "Index": button.index,
        "DeviceLabel": button.device_label,
        "LabelConfigurable": button.label_configurable,
        "Label": button.label,
        "Profile": button.profile,
        "ProfileOptions": button.profile_options,
    })
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn render_options(options: &BTreeMap<String, Value>) -> String {
    if options.is_empty() {
        return String::new();
    }

    options
        .iter()
        .map(|(key, value)| format!("{key}={}", render_value(value)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_value(value: &Value) -> String {
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

fn label_profile_diverges(button: &ButtonView) -> bool {
    !button.device_label.is_empty()
        && !button.profile.is_empty()
        && profile_hint(&button.device_label).is_some_and(|hint| hint != button.profile)
}

fn profile_hint(label: &str) -> Option<&'static str> {
    let lowered = label.to_ascii_lowercase();
    if lowered.contains("ocr") {
        Some("ocr")
    } else if lowered.contains("e-mail") || lowered.contains("email") {
        Some("email")
    } else if lowered.contains("image") {
        Some("image")
    } else if lowered.contains("file") || lowered.contains("document") {
        Some("document")
    } else {
        None
    }
}

fn is_gone(error: &Error) -> bool {
    let message = error.to_string();
    message.contains("org.freedesktop.DBus.Error.UnknownObject")
        || message.contains("org.freedesktop.DBus.Error.UnknownInterface")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_parsing_only_coerces_booleans_and_bare_integers() {
        let parsed = parse_options(
            &[
                "pages=2".to_owned(),
                "flag=true".to_owned(),
                "name=document".to_owned(),
                "gamma=2.2".to_owned(),
            ],
            &[],
        )
        .unwrap();

        assert_eq!(parsed["pages"], Value::U64(2));
        assert_eq!(parsed["flag"], Value::Bool(true));
        assert_eq!(parsed["name"], Value::Str("document".to_owned()));
        assert_eq!(parsed["gamma"], Value::Str("2.2".to_owned()));
    }

    #[test]
    fn option_json_accepts_structures_and_still_expands_tilde_strings() {
        let previous = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", "/tmp/scanbus-home");
        }

        let parsed = parse_options(
            &[],
            &["dir=\"~/Scans\"".to_owned(), "pages=[1,2]".to_owned()],
        )
        .unwrap();

        assert_eq!(
            parsed["dir"],
            Value::Str("/tmp/scanbus-home/Scans".to_owned())
        );
        assert_eq!(
            parsed["pages"],
            Value::Array(vec![Value::U64(1), Value::U64(2)])
        );

        match previous {
            Some(home) => unsafe { std::env::set_var("HOME", home) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}
