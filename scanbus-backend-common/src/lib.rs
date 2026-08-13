//! Shared helpers for subprocess-driven scanner backends.

#[cfg(feature = "mdns")]
pub mod mdns;

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::stream::BoxStream;
use scanbus_core::{BackendError, PageFormat, RawPage, ScannerId};
use tempfile::TempDir;
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::warn;

pub const DEFAULT_SCANIMAGE_HELPER: &str = "/usr/libexec/scanbus/scanbus-scanimage";
const PAGE_POLL_INTERVAL: Duration = Duration::from_millis(50);

struct ReceiverStream(mpsc::Receiver<Result<RawPage, BackendError>>);

impl futures_core::Stream for ReceiverStream {
    type Item = Result<RawPage, BackendError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().0.poll_recv(cx)
    }
}

#[derive(Debug, Clone)]
pub struct ScanimageConfig {
    pub program: PathBuf,
    pub scanner: ScannerId,
    pub device_name: String,
    pub resolution_dpi: u32,
    pub extra_args: Vec<String>,
}

impl ScanimageConfig {
    pub fn new(scanner: ScannerId, device_name: impl Into<String>) -> Self {
        Self {
            program: PathBuf::from(DEFAULT_SCANIMAGE_HELPER),
            scanner,
            device_name: device_name.into(),
            resolution_dpi: 300,
            extra_args: Vec::new(),
        }
    }
}

pub async fn fetch_pages_via_scanimage(
    config: ScanimageConfig,
) -> Result<BoxStream<'static, Result<RawPage, BackendError>>, BackendError> {
    let tempdir = tempfile::tempdir()
        .map_err(|error| BackendError::Other(format!("cannot create scan spool: {error}")))?;
    let batch_pattern = tempdir.path().join("page-%03d.pnm");

    let mut command = Command::new(&config.program);
    command
        .arg(format!("--device-name={}", config.device_name))
        .arg("--format=pnm")
        .arg(format!("--batch={}", batch_pattern.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for arg in &config.extra_args {
        command.arg(arg);
    }

    let mut child = command
        .spawn()
        .map_err(|error| BackendError::NotReachable {
            scanner: config.scanner.clone(),
            detail: format!("failed to start {}: {error}", config.program.display()),
        })?;
    let Some(mut stderr) = child.stderr.take() else {
        return Err(BackendError::Other(format!(
            "{} did not provide stderr",
            config.program.display()
        )));
    };

    let (sender, receiver) = mpsc::channel(8);
    tokio::spawn(async move {
        let mut stderr_buf = Vec::new();
        let stderr_task = tokio::spawn(async move {
            let _ = stderr.read_to_end(&mut stderr_buf).await;
            stderr_buf
        });

        let outcome = stream_scanimage_pages(&config, &tempdir, &mut child, &sender).await;
        let stderr = stderr_task.await.unwrap_or_default();
        let stderr = String::from_utf8_lossy(&stderr).trim().to_owned();

        if let Err(error) = outcome {
            let _ = sender.send(Err(error)).await;
        } else if let Ok(Some(status)) = child.try_wait()
            && !status.success()
        {
            let detail = if stderr.is_empty() {
                format!(
                    "{} exited with {}",
                    config.program.display(),
                    status
                        .code()
                        .map_or_else(|| "signal".to_owned(), |code| format!("exit code {code}"))
                )
            } else {
                format!(
                    "{} exited with {}: {}",
                    config.program.display(),
                    status
                        .code()
                        .map_or_else(|| "signal".to_owned(), |code| format!("exit code {code}")),
                    stderr
                )
            };
            let _ = sender.send(Err(BackendError::Other(detail))).await;
        }
    });

    Ok(Box::pin(ReceiverStream(receiver)))
}

async fn stream_scanimage_pages(
    config: &ScanimageConfig,
    tempdir: &TempDir,
    child: &mut tokio::process::Child,
    sender: &mpsc::Sender<Result<RawPage, BackendError>>,
) -> Result<(), BackendError> {
    let mut next_page = 1u32;

    loop {
        if sender.is_closed() {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Ok(());
        }

        while let Some(data) = read_complete_page(tempdir.path(), next_page).await? {
            sender
                .send(Ok(RawPage {
                    index: next_page - 1,
                    format: PageFormat::Pnm,
                    resolution_dpi: config.resolution_dpi,
                    data,
                }))
                .await
                .map_err(|_| BackendError::Other("scan page receiver dropped".to_owned()))?;
            next_page += 1;
        }

        match child.try_wait() {
            Ok(Some(_)) => {
                while let Some(data) = read_complete_page(tempdir.path(), next_page).await? {
                    sender
                        .send(Ok(RawPage {
                            index: next_page - 1,
                            format: PageFormat::Pnm,
                            resolution_dpi: config.resolution_dpi,
                            data,
                        }))
                        .await
                        .map_err(|_| {
                            BackendError::Other("scan page receiver dropped".to_owned())
                        })?;
                    next_page += 1;
                }
                return Ok(());
            }
            Ok(None) => tokio::time::sleep(PAGE_POLL_INTERVAL).await,
            Err(error) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(BackendError::Other(format!(
                    "failed to poll {}: {error}",
                    config.program.display()
                )));
            }
        }
    }
}

async fn read_complete_page(dir: &Path, page_number: u32) -> Result<Option<Vec<u8>>, BackendError> {
    let path = dir.join(format!("page-{page_number:03}.pnm"));
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(BackendError::Other(format!(
                "cannot read scanned page {}: {error}",
                path.display()
            )));
        }
    };

    match complete_pnm_len(&bytes) {
        Ok(Some(_)) => {
            if let Err(error) = tokio::fs::remove_file(&path).await {
                warn!(path = %path.display(), %error, "could not remove consumed scanimage page");
            }
            Ok(Some(bytes))
        }
        Ok(None) => Ok(None),
        Err(error) => Err(BackendError::Other(format!(
            "scanimage wrote an invalid PNM page {}: {error}",
            path.display()
        ))),
    }
}

fn complete_pnm_len(bytes: &[u8]) -> Result<Option<usize>, String> {
    if bytes.len() < 3 {
        return Ok(None);
    }
    if bytes[0] != b'P' {
        return Err("missing PNM magic".to_owned());
    }

    let kind = bytes[1];
    let channels = match kind {
        b'4' => 0usize,
        b'5' => 1usize,
        b'6' => 3usize,
        _ => return Err(format!("unsupported PNM kind P{}", char::from(kind))),
    };

    let mut cursor = 2usize;
    let width = next_token(bytes, &mut cursor)?
        .map(|token| {
            token
                .parse::<usize>()
                .map_err(|_| format!("invalid width {token:?}"))
        })
        .transpose()?;
    let Some(width) = width else {
        return Ok(None);
    };
    let height = next_token(bytes, &mut cursor)?
        .map(|token| {
            token
                .parse::<usize>()
                .map_err(|_| format!("invalid height {token:?}"))
        })
        .transpose()?;
    let Some(height) = height else {
        return Ok(None);
    };

    let maxval = if kind == b'4' {
        1usize
    } else {
        let maxval = next_token(bytes, &mut cursor)?
            .map(|token| {
                token
                    .parse::<usize>()
                    .map_err(|_| format!("invalid maxval {token:?}"))
            })
            .transpose()?;
        let Some(maxval) = maxval else {
            return Ok(None);
        };
        maxval
    };

    if cursor >= bytes.len() {
        return Ok(None);
    }
    if !bytes[cursor].is_ascii_whitespace() {
        return Err("missing raster separator".to_owned());
    }
    let raster = cursor + 1;
    let payload = if kind == b'4' {
        width.div_ceil(8) * height
    } else {
        let sample_bytes = if maxval < 256 { 1 } else { 2 };
        width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(channels))
            .and_then(|samples| samples.checked_mul(sample_bytes))
            .ok_or_else(|| "raster size overflow".to_owned())?
    };
    let total = raster
        .checked_add(payload)
        .ok_or_else(|| "page size overflow".to_owned())?;

    if bytes.len() < total {
        Ok(None)
    } else {
        Ok(Some(total))
    }
}

fn next_token<'a>(bytes: &'a [u8], cursor: &mut usize) -> Result<Option<&'a str>, String> {
    loop {
        while *cursor < bytes.len() && bytes[*cursor].is_ascii_whitespace() {
            *cursor += 1;
        }
        if *cursor >= bytes.len() {
            return Ok(None);
        }
        if bytes[*cursor] == b'#' {
            while *cursor < bytes.len() && bytes[*cursor] != b'\n' {
                *cursor += 1;
            }
            continue;
        }
        break;
    }

    let start = *cursor;
    while *cursor < bytes.len() && !bytes[*cursor].is_ascii_whitespace() {
        *cursor += 1;
    }
    std::str::from_utf8(&bytes[start..*cursor])
        .map(Some)
        .map_err(|error| format!("header is not valid UTF-8: {error}"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use futures_util::StreamExt as _;

    use super::*;

    fn scanner() -> ScannerId {
        ScannerId::from_backend("hplip", "192.168.1.9").unwrap()
    }

    #[tokio::test]
    async fn fetch_pages_streams_batch_files_in_order() {
        let tempdir = tempfile::tempdir().unwrap();
        let script = tempdir.path().join("scanimage.sh");
        fs::write(
            &script,
            "#!/bin/sh\n\
batch=\n\
for arg in \"$@\"; do\n\
  case \"$arg\" in\n\
    --batch=*) batch=${arg#--batch=} ;;\n\
  esac\n\
done\n\
printf 'P5\\n1 1\\n255\\n\\001' > \"$(printf \"$batch\" 1)\"\n\
sleep 0.1\n\
printf 'P5\\n1 1\\n255\\n\\002' > \"$(printf \"$batch\" 2)\"\n",
        )
        .unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).unwrap();

        let mut config = ScanimageConfig::new(scanner(), "hp:/net/test?ip=192.168.1.9");
        config.program = script;
        let mut stream = fetch_pages_via_scanimage(config).await.unwrap();

        let first = stream.next().await.unwrap().unwrap();
        let second = stream.next().await.unwrap().unwrap();

        assert_eq!(first.index, 0);
        assert_eq!(second.index, 1);
        assert_eq!(first.data.last().copied(), Some(1));
        assert_eq!(second.data.last().copied(), Some(2));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn fetch_pages_reports_subprocess_failure_as_stream_error() {
        let tempdir = tempfile::tempdir().unwrap();
        let script = tempdir.path().join("scanimage-fail.sh");
        fs::write(
            &script,
            "#!/bin/sh\n\
echo 'ADF jam' >&2\n\
exit 7\n",
        )
        .unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).unwrap();

        let mut config = ScanimageConfig::new(scanner(), "hp:/net/test?ip=192.168.1.9");
        config.program = script;
        let mut stream = fetch_pages_via_scanimage(config).await.unwrap();
        let error = stream.next().await.unwrap().unwrap_err();

        assert!(matches!(error, BackendError::Other(_)));
        assert!(error.to_string().contains("ADF jam"), "{error}");
        assert!(stream.next().await.is_none());
    }

    #[test]
    fn complete_pnm_waits_for_the_full_raster() {
        let partial = b"P5\n1 1\n255\n";
        let full = b"P5\n1 1\n255\n\x7f";

        assert_eq!(complete_pnm_len(partial).unwrap(), None);
        assert_eq!(complete_pnm_len(full).unwrap(), Some(full.len()));
    }
}
