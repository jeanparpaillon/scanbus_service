use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;

use scanbus_client::convert;
use scanbus_client::proxy::{Button1Proxy, Profile1Proxy, object_manager};
use scanbus_client::{
    Button, Connection, Error as ClientError, ObjectKind, Objects, ScanbusError, SelectError,
};
use scanbus_core::{ProfileKind, Value, path};
use zbus::proxy::CacheProperties;

use crate::context::Context;
use crate::error::{Error, Result};
use crate::output::{self, Format};

use super::options::{parse_options, render_options};

pub async fn list(context: &Context) -> Result<u8> {
    let connection = context.connect().await?;
    let profiles = read_profiles(context, &connection).await?;
    report_list(context, &profiles)?;
    Ok(0)
}

pub async fn show(context: &Context, name: &str) -> Result<u8> {
    let connection = context.connect().await?;
    let kind = resolve_profile(context, &connection, name).await?;
    let profile = read_profile(context, &connection, kind).await?;
    let overrides = read_overrides(context, &connection, kind).await?;
    report_show(context, &profile, &overrides)?;
    Ok(0)
}

pub async fn set(
    context: &Context,
    name: &str,
    options: &[String],
    option_json: &[String],
) -> Result<u8> {
    let connection = context.connect().await?;
    let kind = resolve_profile(context, &connection, name).await?;
    let proxy = profile_proxy(context, &connection, kind).await?;
    let mut merged = read_profile(context, &connection, kind).await?.options;
    let parsed = parse_options(options, option_json)?;
    for (key, value) in parsed {
        merged.insert(key, value);
    }

    warn_about_output_dir(&merged);

    context
        .within(format!("setting profile options on {name}"), async {
            proxy
                .set_options(convert::to_dict(&merged))
                .await
                .map_err(ClientError::from)
        })
        .await?;

    let profile = read_profile(context, &connection, kind).await?;
    let overrides = read_overrides(context, &connection, kind).await?;
    report_show(context, &profile, &overrides)?;
    Ok(0)
}

#[derive(Debug, Clone, PartialEq)]
struct ProfileView {
    name: String,
    options: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
struct ButtonOverrideView {
    scanner: String,
    index: u32,
    device_label: String,
    options: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
struct ButtonView {
    profile: String,
    options: BTreeMap<String, Value>,
}

async fn read_profiles(context: &Context, connection: &Connection) -> Result<Vec<ProfileView>> {
    let mut profiles = Vec::new();
    for kind in exported_profiles(context, connection).await? {
        profiles.push(read_profile(context, connection, kind).await?);
    }
    Ok(profiles)
}

async fn read_profile(
    context: &Context,
    connection: &Connection,
    kind: ProfileKind,
) -> Result<ProfileView> {
    let proxy = profile_proxy(context, connection, kind).await?;
    let what = format!("reading profile {}", kind.as_str());

    let name = context
        .within(what.clone(), async {
            proxy.name().await.map_err(ClientError::from)
        })
        .await?;
    let options = context
        .within(what.clone(), async {
            proxy.options().await.map_err(ClientError::from)
        })
        .await?;

    Ok(ProfileView {
        name,
        options: convert::from_dict(&options)
            .map_err(ClientError::from)
            .map_err(|error| Error::call(what, error))?,
    })
}

async fn profile_proxy(
    context: &Context,
    connection: &Connection,
    kind: ProfileKind,
) -> Result<Profile1Proxy<'static>> {
    context
        .within(format!("resolving profile {}", kind.as_str()), async {
            Profile1Proxy::for_profile(connection, kind).await
        })
        .await
}

async fn exported_profiles(context: &Context, connection: &Connection) -> Result<Vec<ProfileKind>> {
    let managed = context
        .within("listing the daemon's objects", async {
            let manager = object_manager(connection).await?;
            manager
                .get_managed_objects()
                .await
                .map_err(ClientError::from)
        })
        .await?;

    let mut profiles = Vec::new();
    for (object_path, interfaces) in managed {
        if !interfaces.contains_key("org.scanbus.Profile1") {
            continue;
        }
        let Some(name) = object_path.as_str().strip_prefix("/org/scanbus/profile/") else {
            continue;
        };
        let Ok(kind) = name.parse::<ProfileKind>() else {
            continue;
        };
        profiles.push(kind);
    }

    profiles.sort();
    Ok(profiles)
}

async fn resolve_profile(
    context: &Context,
    connection: &Connection,
    name: &str,
) -> Result<ProfileKind> {
    let available = exported_profiles(context, connection).await?;
    available
        .iter()
        .copied()
        .find(|kind| kind.as_str() == name)
        .ok_or_else(|| {
            Error::call(
                "finding the profile",
                ClientError::from(SelectError::NotFound {
                    kind: ObjectKind::Profile,
                    selector: name.to_owned(),
                    known: available
                        .iter()
                        .map(|kind| kind.as_str().to_owned())
                        .collect(),
                }),
            )
        })
}

async fn read_overrides(
    context: &Context,
    connection: &Connection,
    profile: ProfileKind,
) -> Result<Vec<ButtonOverrideView>> {
    let objects = context
        .within("listing the daemon's objects", Objects::fetch(connection))
        .await?;
    let mut overrides = Vec::new();

    for button in objects.buttons() {
        let view = read_button(context, connection, button).await?;
        if view.profile != profile.as_str() || view.options.is_empty() {
            continue;
        }
        overrides.push(ButtonOverrideView {
            scanner: button.scanner.as_str().to_owned(),
            index: button.index,
            device_label: button.device_label.clone(),
            options: view.options,
        });
    }

    overrides.sort_by(|a, b| {
        (&a.scanner, a.index, &a.device_label).cmp(&(&b.scanner, b.index, &b.device_label))
    });
    Ok(overrides)
}

async fn read_button(
    context: &Context,
    connection: &Connection,
    button: &Button,
) -> Result<ButtonView> {
    let proxy = context
        .within(
            format!("resolving button {} of {}", button.index, button.scanner),
            async {
                Button1Proxy::builder(connection)
                    .path(button.path())
                    .map_err(ClientError::from)?
                    .cache_properties(CacheProperties::No)
                    .build()
                    .await
                    .map_err(ClientError::from)
            },
        )
        .await?;
    let what = format!("reading button {} of {}", button.index, button.scanner);

    let profile = context
        .within(what.clone(), async {
            proxy.profile().await.map_err(ClientError::from)
        })
        .await?;
    let options = context
        .within(what.clone(), async {
            proxy.profile_options().await.map_err(ClientError::from)
        })
        .await?;

    Ok(ButtonView {
        profile,
        options: convert::from_dict(&options)
            .map_err(ClientError::from)
            .map_err(|error| Error::call(what, error))?,
    })
}

fn warn_about_output_dir(options: &BTreeMap<String, Value>) {
    let Some(Value::Str(path)) = options.get("output_folder") else {
        return;
    };
    if path.trim().is_empty() {
        return;
    }

    let directory = Path::new(path);
    if !directory.exists() {
        eprintln!(
            "scanbus: warning: output directory {:?} does not exist yet; writing anyway",
            path
        );
        return;
    }

    if !directory.is_dir() {
        eprintln!(
            "scanbus: warning: output directory {:?} is not a directory; writing anyway",
            path
        );
        return;
    }

    let writable = directory
        .metadata()
        .is_ok_and(|metadata| !metadata.permissions().readonly());
    if !writable {
        eprintln!(
            "scanbus: warning: output directory {:?} does not look writable; writing anyway",
            path
        );
    }
}

fn report_list(context: &Context, profiles: &[ProfileView]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();

    match context.format {
        Format::Json => output::json(
            &mut stdout,
            &serde_json::Value::Array(profiles.iter().map(profile_json).collect()),
        ),
        Format::Human => {
            let rows = profiles.iter().map(profile_row).collect::<Vec<_>>();
            output::table(&mut stdout, context.style, &["NAME", "OPTIONS"], &rows)
        }
    }
}

fn report_show(
    context: &Context,
    profile: &ProfileView,
    overrides: &[ButtonOverrideView],
) -> Result<()> {
    let mut stdout = std::io::stdout().lock();

    match context.format {
        Format::Json => {
            let mut value = profile_json(profile);
            value["ButtonOverrides"] =
                serde_json::Value::Array(overrides.iter().map(button_override_json).collect());
            output::json(&mut stdout, &value)
        }
        Format::Human => {
            output::fields(
                &mut stdout,
                context.style,
                &[
                    ("name", profile.name.clone()),
                    ("path", path::profile(profile_kind(profile)?)),
                    ("options", render_options(&profile.options)),
                ],
            )?;

            if overrides.is_empty() {
                return Ok(());
            }

            writeln!(stdout).map_err(Error::write)?;
            let rows = overrides
                .iter()
                .map(button_override_row)
                .collect::<Vec<_>>();
            output::table(
                &mut stdout,
                context.style,
                &["SCANNER", "IDX", "DEVICE LABEL", "PROFILE OPTIONS"],
                &rows,
            )
        }
    }
}

fn profile_kind(profile: &ProfileView) -> Result<ProfileKind> {
    profile.name.parse::<ProfileKind>().map_err(|_| {
        Error::call(
            "rendering the profile",
            ClientError::Call(ScanbusError::Other {
                name: "org.scanbus.internal.InvalidProfileName".to_owned(),
                message: format!(
                    "profile {:?} is not one of image/document/email/ocr",
                    profile.name
                ),
            }),
        )
    })
}

fn profile_row(profile: &ProfileView) -> Vec<String> {
    vec![profile.name.clone(), render_options(&profile.options)]
}

fn button_override_row(override_view: &ButtonOverrideView) -> Vec<String> {
    vec![
        override_view.scanner.clone(),
        override_view.index.to_string(),
        override_view.device_label.clone(),
        render_options(&override_view.options),
    ]
}

fn profile_json(profile: &ProfileView) -> serde_json::Value {
    serde_json::json!({
        "Name": profile.name,
        "Options": profile.options,
    })
}

fn button_override_json(override_view: &ButtonOverrideView) -> serde_json::Value {
    serde_json::json!({
        "Scanner": override_view.scanner,
        "Index": override_view.index,
        "DeviceLabel": override_view.device_label,
        "ProfileOptions": override_view.options,
    })
}
