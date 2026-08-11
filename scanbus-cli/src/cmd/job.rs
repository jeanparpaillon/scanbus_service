//! `scanbus job …` — listings, one-shot inspection and streaming follow-up ([8.8]).
//!
//! [8.8]: https://github.com/jeanparpaillon/scanbus_service/issues/36

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;

use futures_util::{FutureExt as _, StreamExt as _};
use scanbus_client::proxy::{self, JOB_INTERFACE};
use scanbus_client::{
    Connection, Error as ClientError, ObjectKind, Objects, PropertyWatch, ScanbusError,
};
use scanbus_core::{JobState, Value};
use tokio::sync::mpsc;
use zbus::names::InterfaceName;
use zbus::zvariant::OwnedObjectPath;

use crate::cli::ScannerFilter;
use crate::context::Context;
use crate::error::{Error, Result};
use crate::output::{self, Format};

use super::job_follow::{self, JobView};

pub async fn list(context: &Context, filter: &ScannerFilter) -> Result<u8> {
    let connection = context.connect().await?;
    let objects = context
        .within("listing the daemon's objects", Objects::fetch(&connection))
        .await?;
    let scanner = filtered_scanner(context, &objects, filter)?;

    let mut jobs = Vec::new();
    for job in objects.jobs() {
        if scanner.is_some_and(|scanner| scanner.id != job.scanner) {
            continue;
        }

        if let Some(view) = fetch_job(context, &connection, &job.path()).await? {
            jobs.push(view);
        }
    }

    report_list(context, &jobs)?;
    Ok(0)
}

pub async fn show(context: &Context, selector: &str) -> Result<u8> {
    let connection = context.connect().await?;
    let objects = context
        .within("listing the daemon's objects", Objects::fetch(&connection))
        .await?;
    let job = objects
        .job(selector)
        .cloned()
        .map_err(|error| Error::call("finding the job", error.into()))?;
    let view = fetch_job_required(context, &connection, &job.path(), &job).await?;
    let detail = detail(&objects, &view);

    report_show(context, &detail)?;
    Ok(0)
}

pub async fn watch(context: &Context, filter: &ScannerFilter, until_done: bool) -> Result<u8> {
    let connection = context.connect().await?;
    let objects = context
        .within("listing the daemon's objects", Objects::fetch(&connection))
        .await?;
    let scanner = filtered_scanner(context, &objects, filter)?;

    let manager = context
        .within(
            "subscribing to new jobs",
            scanbus_client::proxy::object_manager(&connection),
        )
        .await?;
    let mut added = context
        .within("subscribing to new jobs", async {
            manager
                .receive_interfaces_added()
                .await
                .map_err(ClientError::from)
        })
        .await?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut watching = BTreeSet::new();
    for job in objects.jobs() {
        if scanner.is_some_and(|scanner| scanner.id != job.scanner) {
            continue;
        }
        start_watch(&connection, &job.path(), tx.clone(), &mut watching);
    }

    let mut previous: BTreeMap<String, JobView> = BTreeMap::new();
    let sigint = tokio::signal::ctrl_c();
    tokio::pin!(sigint);

    loop {
        tokio::select! {
            signal = added.next() => {
                let Some(signal) = signal else { break; };
                let args = match signal.args() {
                    Ok(args) => args,
                    Err(error) => return Err(Error::call("reading InterfacesAdded", ClientError::Bus(error))),
                };

                if !args.interfaces_and_properties().contains_key(JOB_INTERFACE) {
                    continue;
                }

                let path = args.object_path().as_str().to_owned();
                if scanner.is_some_and(|scanner| !path.starts_with(&scanbus_core::path::scanner(&scanner.id))) {
                    continue;
                }

                start_watch(&connection, &path, tx.clone(), &mut watching);
            }
            item = rx.recv() => {
                let Some(item) = item else { break; };
                let path = item.path.as_str().to_owned();
                print_watch(context, previous.get(&path), &item)?;
                let terminal = item.state.is_terminal();
                let failed = matches!(item.state, JobState::Error(_));
                previous.insert(path, item);

                if until_done && terminal {
                    let job = previous.values().last().expect("just inserted");
                    if !failed && job.result.is_empty() {
                        continue;
                    }
                    if failed {
                        return Err(Error::call(
                            format!("waiting for job {}", job_follow::short_id(&job.path)?),
                            ClientError::Call(ScanbusError::Other {
                                name: "org.scanbus.Error.JobFailed".to_owned(),
                                message: job.state.error().to_owned(),
                            }),
                        ));
                    }
                    return Ok(0);
                }
            }
            _ = &mut sigint => return Ok(130),
        }
    }

    Ok(0)
}

#[derive(Debug)]
struct JobDetail {
    job: JobView,
    button_label: String,
    label_diverges: bool,
}

fn detail(objects: &Objects, job: &JobView) -> JobDetail {
    let scanner = scanbus_core::path::scanner_id(&job.scanner);
    let button_label = objects
        .buttons()
        .iter()
        .find(|button| {
            scanner
                .as_ref()
                .is_some_and(|scanner| &button.scanner == scanner)
                && button.index == job.button as u32
        })
        .map(|button| button.device_label.clone())
        .unwrap_or_default();
    let profile_hint = profile_hint(&button_label);

    JobDetail {
        job: job.clone(),
        label_diverges: !button_label.is_empty()
            && !job.profile.is_empty()
            && profile_hint.is_some_and(|hint| hint != job.profile),
        button_label,
    }
}

async fn fetch_job(
    context: &Context,
    connection: &Connection,
    path: &str,
) -> Result<Option<JobView>> {
    match read_job(context, connection, path).await {
        Ok(job) => Ok(Some(job)),
        Err(error) if is_gone(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

async fn fetch_job_required(
    context: &Context,
    connection: &Connection,
    path: &str,
    job: &scanbus_client::Job,
) -> Result<JobView> {
    match read_job(context, connection, path).await {
        Ok(job_view) => Ok(job_view),
        Err(error) if is_gone(&error) => Err(Error::call(
            format!("reading job {}", job.id),
            ClientError::Vanished {
                kind: ObjectKind::Job,
                name: format!("{} of {}", job.id, job.scanner),
            },
        )),
        Err(error) => Err(error),
    }
}

async fn read_job(context: &Context, connection: &Connection, path: &str) -> Result<JobView> {
    let interface = InterfaceName::try_from(JOB_INTERFACE.to_owned())
        .expect("the Job1 interface name is valid");
    let properties = context
        .within(format!("reading job {path}"), async {
            let proxy = proxy::properties(connection, path).await?;
            proxy.get_all(interface).await.map_err(ClientError::from)
        })
        .await?;
    let path_value =
        OwnedObjectPath::try_from(path.to_owned()).expect("job path from the bus is valid");
    JobView::from_properties(path_value, &properties)
}

fn report_list(context: &Context, jobs: &[JobView]) -> Result<()> {
    match context.format {
        Format::Json => {
            let mut stdout = std::io::stdout().lock();
            let values = jobs.iter().map(JobView::json).collect();
            output::json(&mut stdout, &serde_json::Value::Array(values))
        }
        Format::Human => {
            if jobs.is_empty() {
                eprintln!("scanbus: the daemon has no jobs to list right now");
                return Ok(());
            }

            let mut stdout = std::io::stdout().lock();
            let rows = jobs.iter().map(list_row).collect::<Vec<_>>();
            output::table(
                &mut stdout,
                context.style,
                &["ID", "SCANNER", "BUTTON", "PROFILE", "STATE", "PAGES"],
                &rows,
            )
        }
    }
}

fn report_show(context: &Context, detail: &JobDetail) -> Result<()> {
    let mut stdout = std::io::stdout().lock();

    match context.format {
        Format::Json => {
            let mut value = detail.job.json();
            value["ButtonDeviceLabel"] = serde_json::Value::String(detail.button_label.clone());
            value["ButtonLabelDivergesFromProfile"] =
                serde_json::Value::Bool(detail.label_diverges);
            output::json(&mut stdout, &value)
        }
        Format::Human => output::fields(
            &mut stdout,
            context.style,
            &[
                ("path", detail.job.path.as_str().to_owned()),
                ("scanner", detail.job.scanner.clone()),
                ("button", detail.job.button.to_string()),
                ("button label", detail.button_label.clone()),
                ("profile", detail.job.profile.clone()),
                (
                    "label/profile",
                    if detail.label_diverges {
                        "diverges".to_owned()
                    } else {
                        String::new()
                    },
                ),
                ("state", detail.job.state.as_str().to_owned()),
                ("pages", detail.job.page_count.to_string()),
                ("error", detail.job.state.error().to_owned()),
                ("result", render_result(&detail.job.result)),
            ],
        ),
    }
}

fn list_row(job: &JobView) -> Vec<String> {
    vec![
        job_follow::short_id(&job.path)
            .map(|id| id.to_string())
            .unwrap_or_default(),
        job.scanner.clone(),
        job.button.to_string(),
        empty_raw(&job.profile),
        job.state.as_str().to_owned(),
        job.page_count.to_string(),
    ]
}

fn print_watch(context: &Context, previous: Option<&JobView>, job: &JobView) -> Result<()> {
    let mut stdout = std::io::stdout().lock();

    match context.format {
        Format::Json => output::json(&mut stdout, &job.json()),
        Format::Human => {
            let id = job_follow::short_id(&job.path)?;
            let mut parts = vec![format!("job {id}")];

            if previous.is_none() {
                parts.push(format!("scanner={}", job.scanner));
                parts.push(format!("button={}", job.button));
                parts.push(format!("profile={}", empty_raw(&job.profile)));
            }

            parts.push(job.state.as_str().to_owned());

            if previous.is_none_or(|before| before.page_count != job.page_count) {
                parts.push(format!("pages={}", job.page_count));
            }

            if job.state.is_terminal() {
                for (key, value) in &job.result {
                    parts.push(format!("{key}={}", render_value(value)));
                }
            }

            writeln!(&mut stdout, "{}", parts.join("  ")).map_err(Error::write)
        }
    }
}

fn start_watch(
    connection: &Connection,
    path: &str,
    tx: mpsc::UnboundedSender<JobView>,
    watching: &mut BTreeSet<String>,
) {
    if !watching.insert(path.to_owned()) {
        return;
    }

    let connection = connection.clone();
    let path = path.to_owned();
    tokio::spawn(async move {
        let Ok(path_value) = OwnedObjectPath::try_from(path.clone()) else {
            return;
        };
        let Ok(watch) =
            PropertyWatch::subscribe(&connection, path_value.clone(), JOB_INTERFACE).await
        else {
            return;
        };
        let Ok((snapshot, mut changes)) = watch.snapshot().await else {
            return;
        };
        let Ok(mut job) = JobView::from_properties(path_value, &snapshot) else {
            return;
        };
        if tx.send(job.clone()).is_err() {
            return;
        }

        while let Some(signal) = changes.next().await {
            let Ok(args) = signal.args() else {
                return;
            };
            if !args.invalidated_properties.is_empty() {
                return;
            }
            if job.apply(&args.changed_properties).is_err() {
                return;
            }
            if job.state.is_terminal() {
                while let Some(Some(signal)) = changes.next().now_or_never() {
                    let Ok(args) = signal.args() else {
                        return;
                    };
                    if !args.invalidated_properties.is_empty() {
                        return;
                    }
                    if job.apply(&args.changed_properties).is_err() {
                        return;
                    }
                }
            }
            if tx.send(job.clone()).is_err() {
                return;
            }
        }
    });
}

fn filtered_scanner<'a>(
    _context: &Context,
    objects: &'a Objects,
    filter: &ScannerFilter,
) -> Result<Option<&'a scanbus_client::Scanner>> {
    match &filter.scanner {
        Some(selector) => objects
            .scanner(selector, filter.matching())
            .map(Some)
            .map_err(|error| Error::call("finding the scanner", error.into())),
        None => Ok(None),
    }
}

fn empty_raw(text: &str) -> String {
    if text.is_empty() {
        "raw".to_owned()
    } else {
        text.to_owned()
    }
}

fn render_result(result: &BTreeMap<String, Value>) -> String {
    if result.is_empty() {
        return String::new();
    }

    result
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
        Value::Array(items) => items.iter().map(render_value).collect::<Vec<_>>().join(","),
        Value::Dict(entries) => entries
            .iter()
            .map(|(key, value)| format!("{key}:{}", render_value(value)))
            .collect::<Vec<_>>()
            .join(","),
    }
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
