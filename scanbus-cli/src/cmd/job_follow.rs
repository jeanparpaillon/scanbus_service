//! Following one `Job1` object to a terminal state, and reporting what it produced.
//!
//! `scan` in [8.7] needs the same "read the job now, then every change after it" flow as
//! `job watch` in [8.8]. Keeping that in one module now is what stops [8.8] from having
//! to re-implement the terminal-state logic, the result decoding, and the human/JSON
//! rendering a second time.
//!
//! [8.7]: https://github.com/jeanparpaillon/scanbus_service/issues/35
//! [8.8]: https://github.com/jeanparpaillon/scanbus_service/issues/36

use std::collections::BTreeMap;
use std::io::Write as _;

use futures_util::StreamExt as _;
use scanbus_client::convert::{Dict, from_dict};
use scanbus_client::proxy::JOB_INTERFACE;
use scanbus_client::{
    Connection, Error as ClientError, PropertyChanges, PropertyWatch, ScanbusError,
};
use scanbus_core::{JobState, Value, path};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value as ZValue};

use crate::context::Context;
use crate::error::{Error, Result};
use crate::output::{self, Format};

/// One `Job1`, decoded into the model types the CLI actually branches on.
#[derive(Debug, Clone, PartialEq)]
pub struct JobView {
    pub path: OwnedObjectPath,
    pub scanner: String,
    pub button: i32,
    pub profile: String,
    pub state: JobState,
    pub page_count: u32,
    pub result: BTreeMap<String, Value>,
}

impl JobView {
    fn from_properties(path: OwnedObjectPath, properties: &Dict) -> Result<Self> {
        let scanner = object_path(properties, "Scanner")?;
        let button = integer(properties, "Button")?;
        let profile = string(properties, "Profile")?;
        let state_name = string(properties, "State")?;
        let page_count = unsigned(properties, "PageCount")?;
        let result = dict(properties, "Result")
            .and_then(|value| from_dict(&value).map_err(decode_job_error))?;
        let error = string(properties, "Error")?;
        let state = JobState::from_dbus(&state_name, &error).map_err(parse_job_error)?;

        Ok(Self {
            path,
            scanner,
            button,
            profile,
            state,
            page_count,
            result,
        })
    }

    fn apply(&mut self, changed: &std::collections::HashMap<&str, ZValue<'_>>) -> Result<()> {
        let mut state_name = self.state.as_str().to_owned();
        let mut error = self.state.error().to_owned();

        for (name, value) in changed {
            match *name {
                "State" => state_name = borrowed_string(value, "State")?,
                "Error" => error = borrowed_string(value, "Error")?,
                "PageCount" => self.page_count = borrowed_unsigned(value, "PageCount")?,
                "Result" => {
                    let result = borrowed_dict(value, "Result")?;
                    self.result = from_dict(&result).map_err(decode_job_error)?;
                }
                _ => {}
            }
        }

        self.state = JobState::from_dbus(&state_name, &error).map_err(parse_job_error)?;
        Ok(())
    }

    fn terminal_paths(&self) -> Vec<String> {
        match (self.result.get("path"), self.result.get("paths")) {
            (Some(Value::Str(path)), _) => vec![path.clone()],
            (_, Some(Value::Array(paths))) => paths
                .iter()
                .filter_map(|value| match value {
                    Value::Str(path) => Some(path.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "Path": self.path.as_str(),
            "Scanner": self.scanner,
            "Button": self.button,
            "Profile": self.profile,
            "State": self.state.as_str(),
            "PageCount": self.page_count,
            "Result": self.result,
            "Error": self.state.error(),
        })
    }
}

/// The short id the CLI prints for `scan --no-wait`.
pub fn short_id(path_value: &OwnedObjectPath) -> Result<u64> {
    path::job_id(path_value.as_str())
        .map(|(_, id)| id)
        .ok_or_else(|| {
            Error::call(
                "reading the created job id",
                ClientError::Call(ScanbusError::Other {
                    name: "org.scanbus.internal.InvalidJobPath".to_owned(),
                    message: format!("{path_value} is not a valid org.scanbus.Job1 path"),
                }),
            )
        })
}

/// Prints the short id of the created job, in the format §3's `scan --no-wait` needs.
pub fn print_short_id(context: &Context, path_value: &OwnedObjectPath) -> Result<()> {
    let id = short_id(path_value)?;

    if context.format == Format::Json {
        let mut stdout = std::io::stdout().lock();
        return output::json(
            &mut stdout,
            &serde_json::json!({
                "Path": path_value.as_str(),
                "JobId": id,
            }),
        );
    }

    writeln!(std::io::stdout().lock(), "{id}").map_err(Error::write)
}

/// Follows `path` to its terminal `State`, printing the final document under `--json`.
pub async fn follow(
    context: &Context,
    connection: &Connection,
    path_value: OwnedObjectPath,
) -> Result<u8> {
    let watch: PropertyWatch = context
        .within(
            format!("subscribing to job {}", path_value.as_str()),
            PropertyWatch::subscribe(connection, path_value.clone(), JOB_INTERFACE),
        )
        .await?;
    let (snapshot, mut changes): (Dict, PropertyChanges) = context
        .within(
            format!("reading job {}", path_value.as_str()),
            watch.snapshot(),
        )
        .await?;
    let mut job = JobView::from_properties(path_value.clone(), &snapshot)?;

    if job.state.is_terminal() {
        return finish(context, &job);
    }

    loop {
        let Some(signal) = changes.next().await else {
            return Err(stream_ended(&job));
        };

        let args = signal.args().map_err(|error| {
            Error::call(format!("reading job {}", job.path.as_str()), error.into())
        })?;

        if let Some(property) = args.invalidated_properties.first() {
            return Err(Error::call(
                format!("reading job {}", job.path.as_str()),
                ClientError::Invalidated {
                    property: (*property).to_owned(),
                },
            ));
        }

        job.apply(&args.changed_properties)?;
        if job.state.is_terminal() {
            return finish(context, &job);
        }
    }
}

fn finish(context: &Context, job: &JobView) -> Result<u8> {
    if let JobState::Error(message) = &job.state {
        return Err(Error::call(
            format!("waiting for job {}", short_id(&job.path)?),
            ClientError::Call(ScanbusError::Other {
                name: "org.scanbus.Error.JobFailed".to_owned(),
                message: message.clone(),
            }),
        ));
    }

    if context.format == Format::Json {
        let mut stdout = std::io::stdout().lock();
        output::json(&mut stdout, &job.json())?;
        return Ok(0);
    }

    let mut stdout = std::io::stdout().lock();
    for path_value in job.terminal_paths() {
        writeln!(stdout, "{path_value}").map_err(Error::write)?;
    }
    Ok(0)
}

fn string(properties: &Dict, key: &str) -> Result<String> {
    borrowed_string(get(properties, key)?, key)
}

fn integer(properties: &Dict, key: &str) -> Result<i32> {
    let value = get(properties, key)?;
    let number = scanbus_client::convert::from_variant(&Into::<ZValue<'_>>::into(
        value.try_clone().map_err(|_| wrong_type(key, "i"))?,
    ))
    .map_err(decode_job_error)?
    .as_i64()
    .ok_or_else(|| wrong_type(key, "i"))?;
    i32::try_from(number).map_err(|_| wrong_type(key, "i"))
}

fn unsigned(properties: &Dict, key: &str) -> Result<u32> {
    let value = get(properties, key)?;
    let number = scanbus_client::convert::from_variant(&Into::<ZValue<'_>>::into(
        value.try_clone().map_err(|_| wrong_type(key, "u"))?,
    ))
    .map_err(decode_job_error)?
    .as_u64()
    .ok_or_else(|| wrong_type(key, "u"))?;
    u32::try_from(number).map_err(|_| wrong_type(key, "u"))
}

fn object_path(properties: &Dict, key: &str) -> Result<String> {
    let value = get(properties, key)?;
    match Into::<ZValue<'_>>::into(value.try_clone().map_err(|_| wrong_type(key, "o"))?) {
        ZValue::ObjectPath(path_value) => Ok(path_value.as_str().to_owned()),
        _ => Err(wrong_type(key, "o")),
    }
}

fn dict(properties: &Dict, key: &str) -> Result<Dict> {
    let value = get(properties, key)?;
    match value.try_clone() {
        Ok(cloned) => Dict::try_from(cloned).map_err(|_| wrong_type(key, "a{sv}")),
        Err(_) => Err(wrong_type(key, "a{sv}")),
    }
}

fn get<'a>(properties: &'a Dict, key: &str) -> Result<&'a OwnedValue> {
    properties.get(key).ok_or_else(|| {
        Error::call(
            format!("reading job property {key}"),
            ClientError::Call(ScanbusError::Other {
                name: "org.scanbus.internal.MissingJobProperty".to_owned(),
                message: format!("job property {key:?} is missing from the reply"),
            }),
        )
    })
}

fn borrowed_string(value: &ZValue<'_>, key: &str) -> Result<String> {
    match value {
        ZValue::Str(text) => Ok(text.as_str().to_owned()),
        _ => Err(wrong_type(key, "s")),
    }
}

fn borrowed_unsigned(value: &ZValue<'_>, key: &str) -> Result<u32> {
    let parsed = scanbus_client::convert::from_variant(value)
        .map_err(decode_job_error)?
        .as_u64()
        .ok_or_else(|| wrong_type(key, "u"))?;
    u32::try_from(parsed).map_err(|_| wrong_type(key, "u"))
}

fn borrowed_dict(value: &ZValue<'_>, key: &str) -> Result<Dict> {
    match value.try_clone() {
        Ok(cloned) => Dict::try_from(cloned).map_err(|_| wrong_type(key, "a{sv}")),
        Err(_) => Err(wrong_type(key, "a{sv}")),
    }
}

fn wrong_type(key: &str, expected: &str) -> Error {
    Error::call(
        format!("reading job property {key}"),
        ClientError::Call(ScanbusError::Other {
            name: "org.scanbus.internal.BadJobProperty".to_owned(),
            message: format!("job property {key:?} is not a {expected}"),
        }),
    )
}

fn decode_job_error(error: scanbus_client::DecodeError) -> Error {
    Error::call("decoding the job state", ClientError::Decode(error))
}

fn parse_job_error(error: scanbus_core::ParseError) -> Error {
    Error::call("decoding the job state", ClientError::Decode(error.into()))
}

fn stream_ended(job: &JobView) -> Error {
    Error::call(
        format!("waiting for job {}", job.path.as_str()),
        ClientError::Call(ScanbusError::Other {
            name: "org.scanbus.internal.StreamEnded".to_owned(),
            message: "PropertiesChanged stopped before Job1 reached done or error".to_owned(),
        }),
    )
}
