//! `scanbus connect`, `disconnect` and `scan` ([8.7]).
//!
//! The three commands share one selector resolution path and one local profile check:
//! `GetProfileTypes` is the daemon's own list of what it can run *now*, and rejecting
//! `--profile ocr` here is better than letting a method call fail after the scanner was
//! already resolved and, in `connect`'s case, after a `Connect()` that should never have
//! been attempted.
//!
//! [8.7]: https://github.com/jeanparpaillon/scanbus_service/issues/35

use std::collections::{BTreeMap, HashMap};

use scanbus_client::convert::to_dict;
use scanbus_client::proxy::{Manager1Proxy, Scanner1Proxy};
use scanbus_client::{Connection, Error as ClientError, Objects, ScanbusError, Scanner};
use scanbus_core::Value;
use zbus::zvariant::{OwnedValue, Value as ZValue};

use crate::cli::ScannerArg;
use crate::context::Context;
use crate::error::{Error, Result};

use super::job_follow;

/// `scanbus connect <scanner> [--profile P]`.
pub async fn connect(
    context: &Context,
    scanner: &ScannerArg,
    profile: &Option<String>,
) -> Result<u8> {
    let connection = context.connect().await?;
    let found = resolve(context, &connection, scanner).await?;
    let profile = validate_profile(context, &connection, profile.as_deref()).await?;

    let proxy = context
        .within(
            format!("resolving {}", found.id),
            Scanner1Proxy::for_scanner(&connection, &found.id),
        )
        .await?;

    let mut options = HashMap::new();
    if let Some(profile_name) = profile {
        options.insert(
            "profile".to_owned(),
            OwnedValue::try_from(ZValue::from(profile_name.as_str()))
                .expect("a string profile always fits in a variant"),
        );
    }

    call_with_timeout(context, format!("connecting {}", found.id), async {
        proxy.connect(options).await.map_err(ClientError::from)
    })
    .await
    .map_err(|error| explain_connect_error(error, &found))?;

    Ok(0)
}

/// `scanbus disconnect <scanner>`.
pub async fn disconnect(context: &Context, scanner: &ScannerArg) -> Result<u8> {
    let connection = context.connect().await?;
    let found = resolve(context, &connection, scanner).await?;

    let proxy = context
        .within(
            format!("resolving {}", found.id),
            Scanner1Proxy::for_scanner(&connection, &found.id),
        )
        .await?;

    call_with_timeout(context, format!("disconnecting {}", found.id), async {
        proxy.disconnect().await.map_err(ClientError::from)
    })
    .await?;

    Ok(0)
}

/// `scanbus scan <scanner> [--profile P] [--option k=v]… [--no-wait]`.
pub async fn scan(
    context: &Context,
    scanner: &ScannerArg,
    profile: &Option<String>,
    options: &[String],
    no_wait: bool,
) -> Result<u8> {
    let connection = context.connect().await?;
    let found = resolve(context, &connection, scanner).await?;
    let profile = validate_profile(context, &connection, profile.as_deref()).await?;

    let mut scan_options = parse_options(options)?;
    if let Some(profile_name) = profile {
        scan_options.insert("profile".to_owned(), Value::Str(profile_name));
    }

    let proxy = context
        .within(
            format!("resolving {}", found.id),
            Scanner1Proxy::for_scanner(&connection, &found.id),
        )
        .await?;

    let path_value = call_with_timeout(context, format!("scanning with {}", found.id), async {
        proxy
            .scan(to_dict(&scan_options))
            .await
            .map_err(ClientError::from)
    })
    .await
    .map_err(explain_scan_error)?;

    if no_wait {
        job_follow::print_short_id(context, &path_value)?;
        return Ok(0);
    }

    job_follow::follow(context, &connection, path_value).await
}

async fn resolve(
    context: &Context,
    connection: &Connection,
    scanner: &ScannerArg,
) -> Result<Scanner> {
    let objects = context
        .within("listing the daemon's objects", Objects::fetch(connection))
        .await?;

    objects
        .scanner(&scanner.scanner, scanner.matching())
        .cloned()
        .map_err(|error| Error::call("finding the scanner", error.into()))
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

fn parse_options(options: &[String]) -> Result<BTreeMap<String, Value>> {
    let mut parsed = BTreeMap::new();

    for option in options {
        let Some((key, raw)) = option.split_once('=') else {
            return Err(Error::call(
                "parsing --option",
                ClientError::Call(ScanbusError::Other {
                    name: "org.scanbus.internal.InvalidOption".to_owned(),
                    message: format!("option {option:?} must be written as K=V"),
                }),
            ));
        };

        let value =
            serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::Str(raw.to_owned()));
        parsed.insert(key.to_owned(), value);
    }

    Ok(parsed)
}

async fn call_with_timeout<T>(
    context: &Context,
    what: String,
    call: impl Future<Output = std::result::Result<T, ClientError>>,
) -> Result<T> {
    match tokio::time::timeout(context.timeout, call).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(Error::call(what, error)),
        Err(_elapsed) => Err(Error::timeout(what, context.timeout)),
    }
}

fn explain_connect_error(error: Error, scanner: &Scanner) -> Error {
    match format!("{error}") {
        _ if is_named(&error, "org.scanbus.Error.NotPaired") => Error::call(
            format!("connecting {}", scanner.id),
            ClientError::Call(ScanbusError::NotPaired(format!(
                "{} is not paired; run `scanbus pair {}` first",
                scanner.id, scanner.id
            ))),
        ),
        _ if is_named(&error, "org.scanbus.Error.NotReachable") => Error::call(
            format!("connecting {}", scanner.id),
            ClientError::Call(ScanbusError::NotReachable(format!(
                "{} is paired but offline; power it on or fix its connection",
                scanner.id
            ))),
        ),
        _ => error,
    }
}

fn explain_scan_error(error: Error) -> Error {
    if is_named(&error, "org.freedesktop.DBus.Error.UnknownMethod") {
        return Error::call(
            "starting a host-driven scan",
            ClientError::Call(ScanbusError::Other {
                name: "org.freedesktop.DBus.Error.UnknownMethod".to_owned(),
                message: "this daemon does not support host-driven scanning".to_owned(),
            }),
        );
    }

    error
}

fn is_named(error: &Error, name: &str) -> bool {
    error.to_string().contains(name)
}
