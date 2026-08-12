//! HP backend: discovery and install checks through the HPLIP command-line tools.
//!
//! Issue 6.1 deliberately stops short of button delivery and page capture. HPLIP
//! already knows how to enumerate supported devices, their transport, and whether a
//! proprietary plugin is part of the bargain, so this backend shells out to those
//! binaries first and leaves the deeper `hpssd` and scan-transfer work to later issues.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use futures_util::stream;
use scanbus_core::{
    BackendError, ButtonsCapability, Capabilities, PairingProgress, ProfileKind, RawPage,
    ScannerBackend, ScannerId, ScannerInfo, Source, Status, Value,
};
use tokio::sync::mpsc;
use tracing::warn;

/// Backend identifier, as it will be reported by [`ScannerBackend::id`].
pub const ID: &str = "hplip";

const DISCOVERY_ID: &str = "discovery";
const HPLIP_PACKAGE: &str = "hplip";
const HPAIO_PACKAGE: &str = "libsane-hpaio";
const PROPRIETARY_PLUGIN_PACKAGE: &str = "hp-plugin";
const HPLIP_SCAN_REASON_MASK: u32 = 0x40 | 0x80 | 0x100;

/// Walk-up HP backend backed by `hp-probe` and HPLIP's model database.
#[derive(Debug, Clone)]
pub struct HplipBackend {
    hp_probe_path: PathBuf,
    dpkg_query_path: PathBuf,
    models_dat_path: PathBuf,
}

impl Default for HplipBackend {
    fn default() -> Self {
        Self {
            hp_probe_path: PathBuf::from("/usr/bin/hp-probe"),
            dpkg_query_path: PathBuf::from("/usr/bin/dpkg-query"),
            models_dat_path: PathBuf::from("/usr/share/hplip/data/models/models.dat"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProbeRecord {
    device_uri: String,
    model_name: String,
    display_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ModelInfo {
    scan_type: i32,
    scan_src: u32,
    plugin: i32,
    plugin_reason: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DependencyState {
    package: &'static str,
    installed: bool,
}

impl HplipBackend {
    fn discovery_scanner() -> ScannerId {
        ScannerId::from_backend(ID, DISCOVERY_ID).expect("static discovery id is valid")
    }

    fn discover_once(&self) -> Result<Vec<ScannerInfo>, BackendError> {
        if !self.hp_probe_path.exists() {
            warn!(
                path = %self.hp_probe_path.display(),
                "hp-probe is absent; treating HPLIP discovery as empty"
            );
            return Ok(Vec::new());
        }

        let output = Command::new(&self.hp_probe_path)
            .args([
                "--bus=net,usb",
                "--filter=scan",
                "--timeout=5",
                "--ttl=4",
                "--logging=info",
            ])
            .output()
            .map_err(|error| BackendError::NotReachable {
                scanner: Self::discovery_scanner(),
                detail: format!("failed to run hp-probe: {error}"),
            })?;

        if !output.status.success() {
            return Err(BackendError::NotReachable {
                scanner: Self::discovery_scanner(),
                detail: format!(
                    "hp-probe failed with {}: {}",
                    output
                        .status
                        .code()
                        .map_or_else(|| "signal".to_owned(), |code| format!("exit code {code}")),
                    command_output(&output.stdout, &output.stderr)
                ),
            });
        }

        let models = load_models(&self.models_dat_path).unwrap_or_else(|error| {
            warn!(
                path = %self.models_dat_path.display(),
                %error,
                "could not read HPLIP model metadata; discovery will continue without it"
            );
            BTreeMap::new()
        });

        parse_probe_output(&String::from_utf8_lossy(&output.stdout))
            .into_iter()
            .map(|record| scanner_from_probe(record, &models))
            .collect()
    }

    fn ensure_installed_once(&self, scanner: &ScannerInfo) -> Result<(), BackendError> {
        let missing = dependency_states(&self.dpkg_query_path)?
            .into_iter()
            .find(|state| !state.installed);
        if let Some(state) = missing {
            return Err(BackendError::InstallFailed {
                package: state.package.to_owned(),
                detail: format!(
                    "{package} is not installed; install it with your distribution package manager",
                    package = state.package
                ),
            });
        }

        if requires_plugin(scanner) {
            return Err(BackendError::InstallFailed {
                package: PROPRIETARY_PLUGIN_PACKAGE.to_owned(),
                detail: "this model requires HP's proprietary plugin for scanning; install it explicitly if you want to use this backend".to_owned(),
            });
        }

        Ok(())
    }
}

#[async_trait]
impl ScannerBackend for HplipBackend {
    fn id(&self) -> &'static str {
        ID
    }

    async fn discover(&self) -> Result<Vec<ScannerInfo>, BackendError> {
        let backend = self.clone();
        tokio::task::spawn_blocking(move || backend.discover_once())
            .await
            .map_err(|error| BackendError::Other(format!("hplip discover task failed: {error}")))?
    }

    async fn ensure_installed(
        &self,
        scanner: &ScannerInfo,
        progress: mpsc::Sender<PairingProgress>,
    ) -> Result<(), BackendError> {
        let _ = progress
            .send(PairingProgress::Checking {
                message: format!("checking HPLIP dependencies for {}", scanner.name),
            })
            .await;

        match self.ensure_installed_once(scanner) {
            Ok(()) => {
                let _ = progress.send(PairingProgress::Ready).await;
                Ok(())
            }
            Err(error) => {
                let _ = progress
                    .send(PairingProgress::Failed {
                        message: error.to_string(),
                    })
                    .await;
                Err(error)
            }
        }
    }

    async fn start_listening(
        &self,
        _scanner: &ScannerInfo,
    ) -> Result<BoxStream<'static, scanbus_core::ScanTrigger>, BackendError> {
        // Button delivery lands in 6.2; for 6.1 pairing must still be able to finish.
        Ok(Box::pin(stream::pending()))
    }

    async fn stop_listening(&self, _scanner_id: &ScannerId) -> Result<(), BackendError> {
        Ok(())
    }

    async fn set_button_mapping(
        &self,
        _scanner_id: &ScannerId,
        _button_index: u32,
        _profile: Option<ProfileKind>,
        _options: &BTreeMap<String, Value>,
    ) -> Result<(), BackendError> {
        Err(BackendError::Unsupported {
            backend: ID,
            operation: "set_button_mapping",
        })
    }

    async fn fetch_pages(
        &self,
        scanner_id: &ScannerId,
        trigger_id: &str,
    ) -> Result<BoxStream<'static, Result<RawPage, BackendError>>, BackendError> {
        Err(BackendError::UnknownJob {
            scanner: scanner_id.clone(),
            job: trigger_id.to_owned(),
        })
    }
}

fn parse_probe_output(output: &str) -> Vec<ProbeRecord> {
    let mut records = Vec::new();
    let mut in_table = false;
    let mut expects_name = false;

    for raw_line in output.lines() {
        let stripped = strip_ansi(raw_line);
        let line = stripped.trim_end();
        let trimmed = line.trim();

        if trimmed.is_empty() {
            in_table = false;
            continue;
        }

        if trimmed.starts_with("Device URI") && trimmed.contains("Model") {
            in_table = true;
            expects_name = trimmed.contains("Name");
            continue;
        }

        if !in_table || trimmed.starts_with('-') || trimmed.starts_with("Found ") {
            continue;
        }

        let columns: Vec<&str> = trimmed
            .split("  ")
            .filter(|segment| !segment.trim().is_empty())
            .collect();
        let columns = if expects_name {
            if columns.len() < 3 {
                continue;
            }
            &columns[..3]
        } else {
            if columns.len() < 2 {
                continue;
            }
            &columns[..2]
        };

        let device_uri = columns[0].trim();
        if !device_uri.starts_with("hp:/") {
            continue;
        }

        let model_name = columns[1].trim().to_owned();
        let display_name = if expects_name {
            columns[2].trim().to_owned()
        } else {
            model_name.clone()
        };

        records.push(ProbeRecord {
            device_uri: device_uri.to_owned(),
            model_name,
            display_name,
        });
    }

    records
}

fn scanner_from_probe(
    record: ProbeRecord,
    models: &BTreeMap<String, ModelInfo>,
) -> Result<ScannerInfo, BackendError> {
    let model_key = model_key_from_uri(&record.device_uri);
    let model_info = model_key
        .as_deref()
        .and_then(|key| models.get(key))
        .cloned()
        .unwrap_or_default();

    let address =
        physical_address_from_uri(&record.device_uri).unwrap_or_else(|| record.device_uri.clone());
    let id = ScannerId::from_backend(ID, &address)
        .map_err(|error| BackendError::Other(error.to_string()))?;

    Ok(ScannerInfo {
        id,
        name: record.display_name.clone(),
        backend: ID.to_owned(),
        address,
        capabilities: capabilities_from_model(&record, &model_key, &model_info),
        status: Status::Online,
    })
}

fn capabilities_from_model(
    record: &ProbeRecord,
    model_key: &Option<String>,
    model: &ModelInfo,
) -> Capabilities {
    let mut extra = BTreeMap::new();
    extra.insert(
        "device_uri".to_owned(),
        Value::Str(record.device_uri.clone()),
    );
    extra.insert(
        "model_name".to_owned(),
        Value::Str(record.model_name.clone()),
    );
    extra.insert(
        "plugin_required".to_owned(),
        Value::Bool(model_requires_plugin(model)),
    );
    extra.insert(
        "plugin_reason".to_owned(),
        Value::U64(u64::from(model.plugin_reason)),
    );
    extra.insert(
        "scan_type".to_owned(),
        Value::I64(i64::from(model.scan_type)),
    );
    extra.insert("scan_src".to_owned(), Value::U64(u64::from(model.scan_src)));
    if let Some(model_key) = model_key {
        extra.insert("model_key".to_owned(), Value::Str(model_key.clone()));
    }

    Capabilities {
        sources: sources_from_scan_src(model.scan_src),
        duplex: model.scan_type == 6,
        buttons: ButtonsCapability {
            count: 0,
            label_configurable: false,
        },
        extra: BTreeMap::from([("hplip".to_owned(), Value::Dict(extra))]),
        ..Capabilities::default()
    }
}

fn sources_from_scan_src(scan_src: u32) -> Vec<Source> {
    let mut sources = Vec::new();
    if scan_src & 0x1 != 0 {
        sources.push(Source::Flatbed);
    }
    if scan_src & 0x2 != 0 {
        sources.push(Source::Adf);
    }
    sources
}

fn model_requires_plugin(model: &ModelInfo) -> bool {
    model.plugin > 0
        || (model.plugin_reason & HPLIP_SCAN_REASON_MASK != 0 && model.plugin_reason != 0)
}

fn requires_plugin(scanner: &ScannerInfo) -> bool {
    scanner
        .capabilities
        .extra
        .get("hplip")
        .and_then(as_dict)
        .and_then(|dict| dict.get("plugin_required"))
        .and_then(|value| match value {
            Value::Bool(value) => Some(*value),
            _ => None,
        })
        .unwrap_or(false)
}

fn as_dict(value: &Value) -> Option<&BTreeMap<String, Value>> {
    match value {
        Value::Dict(dict) => Some(dict),
        _ => None,
    }
}

fn model_key_from_uri(device_uri: &str) -> Option<String> {
    let rest = device_uri.strip_prefix("hp:/")?;
    let (_, tail) = rest.split_once('/')?;
    let key = tail.split('?').next()?.trim();
    (!key.is_empty()).then(|| key.to_ascii_lowercase())
}

fn physical_address_from_uri(device_uri: &str) -> Option<String> {
    let query = device_uri.split_once('?')?.1;
    let mut bus = None;
    let mut ip = None;
    let mut hostname = None;
    let mut zc = None;
    let mut serial = None;
    let mut device = None;

    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        match key {
            "bus" => bus = Some(value),
            "ip" => ip = Some(value),
            "hostname" => hostname = Some(value.trim_end_matches('.')),
            "zc" => zc = Some(value.trim_end_matches('.')),
            "serial" => serial = Some(value),
            "device" => device = Some(value),
            _ => {}
        }
    }

    if let Some(host) = ip.or(hostname).or(zc) {
        return Some(host.to_ascii_lowercase());
    }

    if bus == Some("usb") {
        if let Some(serial) = serial {
            return Some(serial.to_owned());
        }
        if let Some(device) = device {
            return Some(device.to_owned());
        }
    }

    None
}

fn dependency_states(dpkg_query_path: &Path) -> Result<Vec<DependencyState>, BackendError> {
    Ok(vec![
        DependencyState {
            package: HPLIP_PACKAGE,
            installed: package_installed(
                dpkg_query_path,
                HPLIP_PACKAGE,
                &[Path::new("/usr/bin/hp-probe")],
            )?,
        },
        DependencyState {
            package: HPAIO_PACKAGE,
            installed: package_installed(
                dpkg_query_path,
                HPAIO_PACKAGE,
                &[
                    Path::new("/usr/lib/x86_64-linux-gnu/sane/libsane-hpaio.so.1"),
                    Path::new("/usr/lib64/sane/libsane-hpaio.so.1"),
                    Path::new("/usr/lib/sane/libsane-hpaio.so.1"),
                ],
            )?,
        },
    ])
}

fn package_installed(
    dpkg_query_path: &Path,
    package: &'static str,
    fallback_paths: &[&Path],
) -> Result<bool, BackendError> {
    if dpkg_query_path.exists() {
        let output = Command::new(dpkg_query_path)
            .args(["-W", "-f=${db:Status-Status}\\n", package])
            .output()
            .map_err(|error| {
                BackendError::Other(format!("failed to run dpkg-query for {package}: {error}"))
            })?;

        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).trim() == "installed");
        }

        return Ok(false);
    }

    Ok(fallback_paths.iter().any(|path| path.exists()))
}

fn load_models(path: &Path) -> Result<BTreeMap<String, ModelInfo>, String> {
    let content = fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let mut models = BTreeMap::new();
    let mut current_section = None::<String>;

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(section) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            current_section = Some(section.trim().to_ascii_lowercase());
            models
                .entry(section.trim().to_ascii_lowercase())
                .or_default();
            continue;
        }

        let Some(section) = current_section.as_ref() else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let entry: &mut ModelInfo = models.entry(section.clone()).or_default();
        match key.trim() {
            "scan-type" => entry.scan_type = value.trim().parse().unwrap_or_default(),
            "scan-src" => entry.scan_src = value.trim().parse().unwrap_or_default(),
            "plugin" => entry.plugin = value.trim().parse().unwrap_or_default(),
            "plugin-reason" => entry.plugin_reason = value.trim().parse().unwrap_or_default(),
            _ => {}
        }
    }

    Ok(models)
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == 0x1b {
            index += 1;
            if index < bytes.len() && bytes[index] == b'[' {
                index += 1;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if byte.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }

        out.push(char::from(bytes[index]));
        index += 1;
    }

    out
}

fn command_output(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(stderr).trim().to_owned();

    match (stdout.is_empty(), stderr.is_empty()) {
        (false, false) => format!("{stdout}; {stderr}"),
        (false, true) => stdout,
        (true, false) => stderr,
        (true, true) => "no output".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hp_probe_network_table_is_parsed() {
        let output = r#"
HP Linux Imaging and Printing System

Device URI                                 Model                   Name
-----------------------------------------  ----------------------  ----------------------
hp:/net/officejet_pro_9010?ip=192.168.1.9  HP OfficeJet Pro 9010  OfficeJet Pro 9010

Found 1 printer(s) on the 'net' bus.
"#;

        assert_eq!(
            parse_probe_output(output),
            vec![ProbeRecord {
                device_uri: "hp:/net/officejet_pro_9010?ip=192.168.1.9".to_owned(),
                model_name: "HP OfficeJet Pro 9010".to_owned(),
                display_name: "OfficeJet Pro 9010".to_owned(),
            }]
        );
    }

    #[test]
    fn hp_probe_usb_table_is_parsed() {
        let output = r#"
Device URI                                              Model
------------------------------------------------------  -----------------------
hp:/usb/officejet_8710_series?serial=CN1234ABCDE        HP OfficeJet 8710 series
"#;

        assert_eq!(
            parse_probe_output(output),
            vec![ProbeRecord {
                device_uri: "hp:/usb/officejet_8710_series?serial=CN1234ABCDE".to_owned(),
                model_name: "HP OfficeJet 8710 series".to_owned(),
                display_name: "HP OfficeJet 8710 series".to_owned(),
            }]
        );
    }

    #[test]
    fn physical_address_prefers_network_identity() {
        assert_eq!(
            physical_address_from_uri("hp:/net/officejet_pro_9010?ip=192.168.1.9"),
            Some("192.168.1.9".to_owned())
        );
        assert_eq!(
            physical_address_from_uri("hp:/net/officejet_pro_9010?hostname=HP123.local."),
            Some("hp123.local".to_owned())
        );
    }

    #[test]
    fn capabilities_follow_scan_src_and_plugin_flags() {
        let record = ProbeRecord {
            device_uri: "hp:/net/officejet_pro_9010?ip=192.168.1.9".to_owned(),
            model_name: "HP OfficeJet Pro 9010".to_owned(),
            display_name: "OfficeJet Pro 9010".to_owned(),
        };
        let capabilities = capabilities_from_model(
            &record,
            &Some("officejet_pro_9010".to_owned()),
            &ModelInfo {
                scan_type: 6,
                scan_src: 0x1 | 0x2,
                plugin: 1,
                plugin_reason: 0x40,
            },
        );

        assert_eq!(capabilities.sources, vec![Source::Flatbed, Source::Adf]);
        assert!(capabilities.duplex);
        assert_eq!(capabilities.buttons.count, 0);
        assert!(requires_plugin(&ScannerInfo {
            id: ScannerId::from_backend(ID, "192.168.1.9").unwrap(),
            name: record.display_name,
            backend: ID.to_owned(),
            address: "192.168.1.9".to_owned(),
            capabilities,
            status: Status::Online,
        }));
    }

    #[test]
    fn models_dat_fields_are_loaded() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("models.dat");
        fs::write(
            &path,
            r#"
[officejet_pro_9010]
scan-type=6
scan-src=3
plugin=1
plugin-reason=64
"#,
        )
        .unwrap();

        let models = load_models(&path).unwrap();
        assert_eq!(
            models.get("officejet_pro_9010"),
            Some(&ModelInfo {
                scan_type: 6,
                scan_src: 3,
                plugin: 1,
                plugin_reason: 64,
            })
        );
    }
}
