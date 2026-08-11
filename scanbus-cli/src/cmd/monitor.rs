//! `scanbus monitor` — raw signal firehose under `/org/scanbus` ([8.8]).
//!
//! [8.8]: https://github.com/jeanparpaillon/scanbus_service/issues/36

use futures_util::StreamExt as _;
use scanbus_client::convert::from_variant;
use scanbus_client::proxy::{BUTTON_INTERFACE, JOB_INTERFACE, SCANNER_INTERFACE};
use std::io::Write as _;
use zbus::message::Type;
use zbus::{MatchRule, MessageStream};

use crate::context::Context;
use crate::error::{Error, Result};
use crate::output::{self, Format};

pub async fn run(context: &Context, prefix: Option<&str>) -> Result<u8> {
    let connection = context.connect().await?;
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .path_namespace(scanbus_core::path::ROOT)
        .map_err(scanbus_client::Error::from)
        .map_err(|error| Error::call("subscribing to /org/scanbus", error))?
        .build();
    let mut stream = context
        .within(
            "subscribing to /org/scanbus",
            async { MessageStream::for_match_rule(rule, &connection, None).await.map_err(scanbus_client::Error::from) },
        )
        .await?;

    loop {
        let Some(message) = stream.next().await else {
            break;
        };
        let message = message.map_err(|error| Error::call("reading /org/scanbus signals", error.into()))?;
        let header = message.header();
        let path = match header.path() {
            Some(path) => path.as_str().to_owned(),
            None => continue,
        };

        if prefix.is_some_and(|prefix| !path.starts_with(prefix)) {
            continue;
        }

        let interface = header
            .interface()
            .map(|name| name.as_str().to_owned())
            .unwrap_or_default();
        let member = header
            .member()
            .map(|name| name.as_str().to_owned())
            .unwrap_or_default();

        if interface == "org.freedesktop.DBus.ObjectManager" && member == "InterfacesAdded" {
            let Some(signal) = zbus::fdo::InterfacesAdded::from_message(message) else {
                continue;
            };
            let args = signal
                .args()
                .map_err(|error| Error::call("reading InterfacesAdded", scanbus_client::Error::Bus(error)))?;
            print_added(context, args.object_path().as_str(), &args.interfaces_and_properties)?;
        } else if interface == "org.freedesktop.DBus.ObjectManager" && member == "InterfacesRemoved" {
            let Some(signal) = zbus::fdo::InterfacesRemoved::from_message(message) else {
                continue;
            };
            let args = signal
                .args()
                .map_err(|error| Error::call("reading InterfacesRemoved", scanbus_client::Error::Bus(error)))?;
            print_removed(context, args.object_path().as_str(), &args.interfaces)?;
        } else if interface == "org.freedesktop.DBus.Properties" && member == "PropertiesChanged" {
            let Some(signal) = zbus::fdo::PropertiesChanged::from_message(message) else {
                continue;
            };
            let args = signal
                .args()
                .map_err(|error| Error::call("reading PropertiesChanged", scanbus_client::Error::Bus(error)))?;
            print_changed(
                context,
                &path,
                args.interface_name.as_str(),
                &args.changed_properties,
                &args.invalidated_properties,
            )?;
        }
    }

    Ok(0)
}

fn print_added(
    context: &Context,
    path: &str,
    interfaces: &std::collections::HashMap<
        zbus::names::InterfaceName<'_>,
        std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
    >,
) -> Result<()> {
    let interfaces = interfaces
        .iter()
        .map(|(name, values)| {
            (
                name.as_str().to_owned(),
                serde_json::Value::Object(
                    values
                        .iter()
                        .map(|(key, value)| ((*key).to_owned(), value_json(value)))
                        .collect(),
                ),
            )
        })
        .collect::<serde_json::Map<String, serde_json::Value>>();

    print_event(
        context,
        serde_json::json!({
            "event": "added",
            "path": path,
            "interfaces": interfaces,
        }),
        format!("added {path} {}", classify_interfaces(interfaces.keys())),
    )
}

fn print_removed(
    context: &Context,
    path: &str,
    interfaces: &[zbus::names::InterfaceName<'_>],
) -> Result<()> {
    let interfaces = interfaces
        .iter()
        .map(|name| serde_json::Value::String(name.as_str().to_owned()))
        .collect::<Vec<_>>();
    print_event(
        context,
        serde_json::json!({
            "event": "removed",
            "path": path,
            "interfaces": interfaces,
        }),
        format!("removed {path}"),
    )
}

fn print_changed(
    context: &Context,
    path: &str,
    interface: &str,
    changed: &std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
    invalidated: &[&str],
) -> Result<()> {
    let changed_json = changed
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value_json(value)))
        .collect::<serde_json::Map<String, serde_json::Value>>();
    let invalidated_json = invalidated
        .iter()
        .map(|key| serde_json::Value::String((*key).to_owned()))
        .collect::<Vec<_>>();

    print_event(
        context,
        serde_json::json!({
            "event": "changed",
            "path": path,
            "interface": interface,
            "changed": changed_json,
            "invalidated": invalidated_json,
        }),
        format!("changed {path} {interface}"),
    )
}

fn print_event(context: &Context, json: serde_json::Value, human: String) -> Result<()> {
    match context.format {
        Format::Json => {
            let mut stdout = std::io::stdout().lock();
            output::json(&mut stdout, &json)
        }
        Format::Human => {
            let mut stdout = std::io::stdout().lock();
            writeln!(&mut stdout, "{human}").map_err(Error::write)
        }
    }
}

fn value_json(value: &zbus::zvariant::Value<'_>) -> serde_json::Value {
    match from_variant(value) {
        Ok(value) => serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
        Err(_) => serde_json::Value::String(value.value_signature().to_string()),
    }
}

fn classify_interfaces<'a>(interfaces: impl Iterator<Item = &'a String>) -> &'static str {
    for interface in interfaces {
        if interface == SCANNER_INTERFACE || interface == BUTTON_INTERFACE || interface == JOB_INTERFACE {
            return interface_type(interface);
        }
    }

    "object"
}

fn interface_type(interface: &str) -> &'static str {
    match interface {
        SCANNER_INTERFACE => "scanner",
        BUTTON_INTERFACE => "button",
        JOB_INTERFACE => "job",
        _ => "object",
    }
}
