//! Brother backend: discovery plus install checks for `brscan4`/`brscan5`.
//!
//! The first implementation intentionally starts from `scanimage -L`: on the target
//! machine it already enumerates Brother devices through the proprietary SANE backend
//! as well as any eSCL-capable ones through airscan, which gives us one probe to parse
//! instead of a fresh SNMP/mDNS implementation before the basics are tested.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use scanbus_core::{
    BackendError, ButtonsCapability, Capabilities, ColorMode, PairingProgress, ProfileKind,
    RawPage, RestoreDisposition, ScanTrigger, ScannerBackend, ScannerId, ScannerInfo, Source,
    Status, Value,
};
use tokio::sync::mpsc;
use tracing::warn;

/// Backend identifier, as it will be reported by [`ScannerBackend::id`].
pub const ID: &str = "brother-skey";

const SCANNER_ID_BACKEND: &str = "brother";
const SCANNER_BACKEND_NAME: &str = "proprietary:brother";
const BRSCAN_SKEY_PACKAGE: &str = "brscan-skey";

/// Walk-up Brother backend backed by `scanimage -L` plus local package checks.
#[derive(Debug, Clone)]
pub struct BrotherBackend {
    scanimage_path: PathBuf,
    dpkg_query_path: PathBuf,
}

impl Default for BrotherBackend {
    fn default() -> Self {
        Self {
            scanimage_path: PathBuf::from("/usr/bin/scanimage"),
            dpkg_query_path: PathBuf::from("/usr/bin/dpkg-query"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Driver {
    Brscan4,
    Brscan5,
}

impl Driver {
    const fn package(self) -> &'static str {
        match self {
            Self::Brscan4 => "brscan4",
            Self::Brscan5 => "brscan5",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    Brother4,
    Brother5,
    Airscan,
    Escl,
    Other,
}

impl Transport {
    const fn is_vendor(self) -> bool {
        matches!(self, Self::Brother4 | Self::Brother5)
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Brother4 => "brother4",
            Self::Brother5 => "brother5",
            Self::Airscan => "airscan",
            Self::Escl => "escl",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Sighting {
    device_uri: String,
    description: String,
    transport: Transport,
    model: Option<String>,
    model_key: Option<String>,
    address_hint: Option<String>,
    stable_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Enrichment {
    address_hint: Option<String>,
    stable_hint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct InstalledState {
    brscan4: bool,
    brscan5: bool,
    brscan_skey: bool,
}

impl InstalledState {
    const fn driver_installed(self, driver: Driver) -> bool {
        match driver {
            Driver::Brscan4 => self.brscan4,
            Driver::Brscan5 => self.brscan5,
        }
    }
}

impl BrotherBackend {
    fn discovery_scanner() -> ScannerId {
        ScannerId::from_backend(SCANNER_ID_BACKEND, "discovery")
            .expect("static discovery id is valid")
    }

    fn discover_once(&self) -> Result<Vec<ScannerInfo>, BackendError> {
        if !self.scanimage_path.exists() {
            warn!(
                path = %self.scanimage_path.display(),
                "scanimage is absent; treating Brother discovery as empty"
            );
            return Ok(Vec::new());
        }

        let output = Command::new(&self.scanimage_path)
            .arg("-L")
            .output()
            .map_err(|error| BackendError::NotReachable {
                scanner: Self::discovery_scanner(),
                detail: format!("failed to run scanimage -L: {error}"),
            })?;

        if !output.status.success() {
            return Err(BackendError::NotReachable {
                scanner: Self::discovery_scanner(),
                detail: format!(
                    "scanimage -L failed with {}: {}",
                    output
                        .status
                        .code()
                        .map_or_else(|| "signal".to_owned(), |code| format!("exit code {code}")),
                    command_output(&output.stdout, &output.stderr)
                ),
            });
        }

        let installed = installed_state(&self.dpkg_query_path)?;
        scanners_from_sightings(parse_scanimage_output(&String::from_utf8_lossy(&output.stdout)), installed)
    }

    fn ensure_installed_once(&self, scanner: &ScannerInfo) -> Result<(), BackendError> {
        let metadata = brother_metadata(scanner).ok_or_else(|| {
            BackendError::InstallFailed {
                package: "brother-driver".to_owned(),
                detail: format!(
                    "scanner {} is missing Brother discovery metadata; rediscover it first",
                    scanner.id
                ),
            }
        })?;
        let driver = metadata.driver.ok_or_else(|| BackendError::InstallFailed {
            package: "brother-driver".to_owned(),
            detail: format!(
                "scanbus does not know whether model {} requires brscan4 or brscan5 yet",
                scanner.name
            ),
        })?;

        let installed = installed_state(&self.dpkg_query_path)?;
        if !installed.driver_installed(driver) {
            return Err(BackendError::InstallFailed {
                package: driver.package().to_owned(),
                detail: format!(
                    "{} is not installed; install it before pairing {}",
                    driver.package(),
                    scanner.name
                ),
            });
        }
        if !installed.brscan_skey {
            return Err(BackendError::InstallFailed {
                package: BRSCAN_SKEY_PACKAGE.to_owned(),
                detail: format!(
                    "{BRSCAN_SKEY_PACKAGE} is not installed; install it before pairing {}",
                    scanner.name
                ),
            });
        }

        Ok(())
    }
}

#[async_trait]
impl ScannerBackend for BrotherBackend {
    fn id(&self) -> &'static str {
        ID
    }

    async fn discover(&self) -> Result<Vec<ScannerInfo>, BackendError> {
        let backend = self.clone();
        tokio::task::spawn_blocking(move || backend.discover_once())
            .await
            .map_err(|error| BackendError::Other(format!("brother discover task failed: {error}")))?
    }

    async fn ensure_installed(
        &self,
        scanner: &ScannerInfo,
        progress: mpsc::Sender<PairingProgress>,
    ) -> Result<(), BackendError> {
        let _ = progress
            .send(PairingProgress::Checking {
                message: format!("checking Brother dependencies for {}", scanner.name),
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
        scanner: &ScannerInfo,
    ) -> Result<BoxStream<'static, ScanTrigger>, BackendError> {
        Err(BackendError::Unsupported {
            backend: ID,
            operation: "start_listening",
        }
        .with_scanner(scanner.id.clone()))
    }

    async fn stop_listening(&self, _scanner_id: &ScannerId) -> Result<(), BackendError> {
        Ok(())
    }

    async fn restore_disposition(&self, _scanner: &ScannerInfo) -> RestoreDisposition {
        RestoreDisposition::Paired
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
        _trigger_id: &str,
    ) -> Result<BoxStream<'static, Result<RawPage, BackendError>>, BackendError> {
        Err(BackendError::UnknownJob {
            scanner: scanner_id.clone(),
            job: "fetch_pages".to_owned(),
        })
    }
}

trait UnsupportedWithScanner {
    fn with_scanner(self, scanner: ScannerId) -> BackendError;
}

impl UnsupportedWithScanner for BackendError {
    fn with_scanner(self, scanner: ScannerId) -> BackendError {
        match self {
            BackendError::Unsupported { backend, operation } => {
                BackendError::Other(format!(
                    "{operation} is not supported by backend {backend} for scanner {scanner}"
                ))
            }
            other => other,
        }
    }
}

fn scanners_from_sightings(
    sightings: Vec<Sighting>,
    installed: InstalledState,
) -> Result<Vec<ScannerInfo>, BackendError> {
    let enrichments = enrichments_by_model(&sightings);
    let mut scanners = BTreeMap::<ScannerId, (u8, ScannerInfo)>::new();

    for sighting in sightings {
        if !is_brother_sighting(&sighting) {
            continue;
        }

        let enrichment = sighting
            .model_key
            .as_ref()
            .and_then(|key| enrichments.get(key))
            .cloned()
            .unwrap_or(Enrichment {
                address_hint: None,
                stable_hint: None,
            });
        let driver = required_driver(&sighting);
        let address = sighting
            .address_hint
            .clone()
            .or(enrichment.address_hint)
            .unwrap_or_else(|| sighting.device_uri.clone());
        let stable_source = sighting
            .stable_hint
            .clone()
            .or(enrichment.stable_hint)
            .unwrap_or_else(|| address.clone());
        let id = ScannerId::from_backend(SCANNER_ID_BACKEND, &stable_source)
            .map_err(|error| BackendError::Other(error.to_string()))?;
        let info = ScannerInfo {
            id: id.clone(),
            name: sighting
                .model
                .clone()
                .unwrap_or_else(|| display_name(&sighting.description)),
            backend: SCANNER_BACKEND_NAME.to_owned(),
            address: address.clone(),
            capabilities: capabilities_for_sighting(&sighting, driver, installed),
            status: Status::Online,
        };
        let precedence = if sighting.transport.is_vendor() { 0 } else { 1 };
        match scanners.get(&id) {
            Some((current_precedence, _)) if *current_precedence <= precedence => {}
            _ => {
                scanners.insert(id, (precedence, info));
            }
        }
    }

    Ok(scanners.into_values().map(|(_, info)| info).collect())
}

fn parse_scanimage_output(output: &str) -> Vec<Sighting> {
    output.lines().filter_map(parse_scanimage_line).collect()
}

fn parse_scanimage_line(line: &str) -> Option<Sighting> {
    let line = line.trim();
    if !line.starts_with("device '") {
        return None;
    }

    let rest = line.strip_prefix("device '")?;
    let (device_uri, description) = rest.split_once("' is ")?;
    let description = description
        .trim()
        .trim_matches('\'')
        .strip_prefix("a ")
        .unwrap_or(description.trim().trim_matches('\''))
        .to_owned();
    let transport = transport_from_uri(device_uri);
    let address_hint = physical_address_from_uri(device_uri);
    let stable_hint = stable_hint_from_uri(device_uri);
    let model = model_from_text(&description).or_else(|| model_from_text(device_uri));
    let model_key = model.as_ref().map(|model| normalize_model(model));

    Some(Sighting {
        device_uri: device_uri.to_owned(),
        description,
        transport,
        model,
        model_key,
        address_hint,
        stable_hint,
    })
}

fn transport_from_uri(uri: &str) -> Transport {
    if uri.starts_with("brother4:") {
        Transport::Brother4
    } else if uri.starts_with("brother5:") {
        Transport::Brother5
    } else if uri.starts_with("airscan:") {
        Transport::Airscan
    } else if uri.starts_with("escl:") || uri.starts_with("http://") || uri.starts_with("https://")
    {
        Transport::Escl
    } else {
        Transport::Other
    }
}

fn is_brother_sighting(sighting: &Sighting) -> bool {
    sighting.transport.is_vendor()
        || contains_brother_marker(&sighting.device_uri)
        || contains_brother_marker(&sighting.description)
}

fn contains_brother_marker(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("brother ") || lower.contains(" brother") || lower.contains("brother_")
}

fn enrichments_by_model(sightings: &[Sighting]) -> BTreeMap<String, Enrichment> {
    let mut addresses = BTreeMap::<String, BTreeSet<String>>::new();
    let mut stable = BTreeMap::<String, BTreeSet<String>>::new();

    for sighting in sightings {
        if sighting.transport.is_vendor() || !is_brother_sighting(sighting) {
            continue;
        }
        let Some(model_key) = sighting.model_key.as_ref() else {
            continue;
        };
        if let Some(address_hint) = sighting.address_hint.as_ref() {
            addresses
                .entry(model_key.clone())
                .or_default()
                .insert(address_hint.clone());
        }
        if let Some(stable_hint) = sighting.stable_hint.as_ref() {
            stable
                .entry(model_key.clone())
                .or_default()
                .insert(stable_hint.clone());
        }
    }

    let mut out = BTreeMap::new();
    for model_key in addresses.keys().chain(stable.keys()) {
        let address_hint = addresses.get(model_key).and_then(unique_entry).cloned();
        let stable_hint = stable.get(model_key).and_then(unique_entry).cloned();
        if address_hint.is_some() || stable_hint.is_some() {
            out.insert(
                model_key.clone(),
                Enrichment {
                    address_hint,
                    stable_hint,
                },
            );
        }
    }
    out
}

fn unique_entry(set: &BTreeSet<String>) -> Option<&String> {
    if set.len() == 1 {
        set.first()
    } else {
        None
    }
}

fn required_driver(sighting: &Sighting) -> Option<Driver> {
    if let Some(model_key) = sighting.model_key.as_deref() {
        if matches!(
            model_key,
            "mfc-l2710dw" | "mfc-l2750dw" | "dcp-l2530dw" | "dcp-l2550dw" | "mfc-l3770cdw"
        ) {
            return Some(Driver::Brscan4);
        }
        if matches!(model_key, "ads-1800w" | "ads-4700w" | "mfc-j1010dw") {
            return Some(Driver::Brscan5);
        }
    }

    match sighting.transport {
        Transport::Brother4 => Some(Driver::Brscan4),
        Transport::Brother5 => Some(Driver::Brscan5),
        _ => None,
    }
}

fn capabilities_for_sighting(
    sighting: &Sighting,
    driver: Option<Driver>,
    installed: InstalledState,
) -> Capabilities {
    let mut capabilities = model_capabilities(sighting.model_key.as_deref()).unwrap_or_default();
    let mut extra = BTreeMap::from([(
        "device_uri".to_owned(),
        Value::Str(sighting.device_uri.clone()),
    )]);
    if let Some(driver) = driver {
        extra.insert("driver".to_owned(), Value::Str(driver.package().to_owned()));
        extra.insert(
            "driver_installed".to_owned(),
            Value::Bool(installed.driver_installed(driver)),
        );
    }
    extra.insert(
        "transport".to_owned(),
        Value::Str(sighting.transport.as_str().to_owned()),
    );
    if let Some(stable_hint) = sighting.stable_hint.as_ref() {
        extra.insert("stable_hint".to_owned(), Value::Str(stable_hint.clone()));
    }
    if let Some(address_hint) = sighting.address_hint.as_ref() {
        extra.insert(
            "physical_address".to_owned(),
            Value::Str(address_hint.clone()),
        );
    }
    if let Some(model) = sighting.model.as_ref() {
        extra.insert("model".to_owned(), Value::Str(model.clone()));
    }

    capabilities
        .extra
        .insert("brother".to_owned(), Value::Dict(extra));
    capabilities
}

fn model_capabilities(model_key: Option<&str>) -> Option<Capabilities> {
    match model_key? {
        "mfc-l2710dw" | "mfc-l2750dw" | "dcp-l2550dw" => Some(Capabilities {
            resolutions: vec![100, 200, 300, 600],
            color_modes: vec![ColorMode::Color, ColorMode::Gray, ColorMode::Bw],
            sources: vec![Source::Flatbed, Source::Adf],
            duplex: true,
            buttons: ButtonsCapability {
                count: 4,
                label_configurable: false,
                labels: Vec::new(),
            },
            ..Capabilities::default()
        }),
        "dcp-l2530dw" => Some(Capabilities {
            resolutions: vec![100, 200, 300, 600],
            color_modes: vec![ColorMode::Color, ColorMode::Gray, ColorMode::Bw],
            sources: vec![Source::Flatbed, Source::Adf],
            duplex: false,
            buttons: ButtonsCapability {
                count: 4,
                label_configurable: false,
                labels: Vec::new(),
            },
            ..Capabilities::default()
        }),
        _ => Some(Capabilities {
            buttons: ButtonsCapability {
                count: 0,
                label_configurable: false,
                labels: Vec::new(),
            },
            ..Capabilities::default()
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrotherMetadata {
    driver: Option<Driver>,
}

fn brother_metadata(scanner: &ScannerInfo) -> Option<BrotherMetadata> {
    let dict = scanner.capabilities.extra.get("brother").and_then(as_dict)?;
    let driver = dict.get("driver").and_then(|value| match value {
        Value::Str(package) if package == "brscan4" => Some(Driver::Brscan4),
        Value::Str(package) if package == "brscan5" => Some(Driver::Brscan5),
        _ => None,
    });
    Some(BrotherMetadata { driver })
}

fn as_dict(value: &Value) -> Option<&BTreeMap<String, Value>> {
    match value {
        Value::Dict(dict) => Some(dict),
        _ => None,
    }
}

fn display_name(description: &str) -> String {
    model_from_text(description).unwrap_or_else(|| description.trim().to_owned())
}

fn model_from_text(text: &str) -> Option<String> {
    for prefix in ["MFC-", "DCP-", "ADS-", "DS-", "HL-"] {
        if let Some(index) = text.to_ascii_uppercase().find(prefix) {
            let tail = &text[index..];
            let len = tail
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .count();
            if len > prefix.len() {
                return Some(tail[..len].to_ascii_uppercase());
            }
        }
    }
    None
}

fn normalize_model(model: &str) -> String {
    model.trim().to_ascii_lowercase()
}

fn physical_address_from_uri(uri: &str) -> Option<String> {
    url_host(uri)
        .or_else(|| query_value(uri, "ip").map(|value| value.to_ascii_lowercase()))
        .or_else(|| query_value(uri, "hostname").map(|value| value.trim_end_matches('.').to_ascii_lowercase()))
        .or_else(|| query_value(uri, "zc").map(|value| value.trim_end_matches('.').to_ascii_lowercase()))
        .or_else(|| usb_endpoint(uri))
        .or_else(|| ipv4(uri))
}

fn stable_hint_from_uri(uri: &str) -> Option<String> {
    query_value(uri, "serial")
        .map(str::to_owned)
        .or_else(|| query_value(uri, "hostname").map(|value| value.trim_end_matches('.').to_ascii_lowercase()))
        .or_else(|| query_value(uri, "zc").map(|value| value.trim_end_matches('.').to_ascii_lowercase()))
        .or_else(|| url_host(uri))
        .or_else(|| usb_endpoint(uri))
}

fn query_value<'a>(uri: &'a str, key: &str) -> Option<&'a str> {
    let query = uri.split_once('?')?.1;
    for pair in query.split('&') {
        let (current_key, value) = pair.split_once('=')?;
        if current_key == key && !value.is_empty() {
            return Some(value);
        }
    }
    None
}

fn usb_endpoint(address: &str) -> Option<String> {
    let start = address.find("usb:")?;
    let rest = &address[start + "usb:".len()..];
    let mut parts = rest.split(':');
    let bus = numeric_prefix(parts.next()?)?;
    let device = numeric_prefix(parts.next()?)?;
    Some(format!("usb:{bus}:{device}"))
}

fn url_host(address: &str) -> Option<String> {
    let start = address.find("://")?;
    let rest = &address[start + "://".len()..];
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty()).then(|| host.trim_end_matches('.').to_ascii_lowercase())
}

fn ipv4(address: &str) -> Option<String> {
    address
        .split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find(|token| {
            let octets: Vec<&str> = token.split('.').collect();
            octets.len() == 4
                && octets
                    .iter()
                    .all(|octet| !octet.is_empty() && octet.parse::<u8>().is_ok())
        })
        .map(str::to_owned)
}

fn numeric_prefix(token: &str) -> Option<&str> {
    let end = token
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(token.len());
    (end > 0).then(|| &token[..end])
}

fn installed_state(dpkg_query_path: &Path) -> Result<InstalledState, BackendError> {
    Ok(InstalledState {
        brscan4: package_installed(
            dpkg_query_path,
            "brscan4",
            &[
                Path::new("/opt/brother/scanner/brscan4"),
                Path::new("/usr/lib64/sane/libsane-brother4.so"),
                Path::new("/usr/lib/x86_64-linux-gnu/sane/libsane-brother4.so"),
            ],
        )?,
        brscan5: package_installed(
            dpkg_query_path,
            "brscan5",
            &[
                Path::new("/opt/brother/scanner/brscan5"),
                Path::new("/usr/lib64/sane/libsane-brother5.so"),
                Path::new("/usr/lib/x86_64-linux-gnu/sane/libsane-brother5.so"),
            ],
        )?,
        brscan_skey: package_installed(
            dpkg_query_path,
            BRSCAN_SKEY_PACKAGE,
            &[
                Path::new("/usr/bin/brscan-skey"),
                Path::new("/opt/brother/scanner/brscan-skey"),
            ],
        )?,
    })
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
    fn scanimage_lines_are_parsed() {
        let output = r#"
device 'brother4:net1;dev0' is 'Brother MFC-L2710DW'
device 'airscan:escl:Brother MFC-L2710DW http://BRW001122334455.local:80/eSCL/' is a WSD or eSCL Brother MFC-L2710DW series
"#;

        let parsed = parse_scanimage_output(output);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].transport, Transport::Brother4);
        assert_eq!(parsed[0].model.as_deref(), Some("MFC-L2710DW"));
        assert_eq!(parsed[1].transport, Transport::Airscan);
        assert_eq!(
            parsed[1].address_hint.as_deref(),
            Some("brw001122334455.local:80")
        );
    }

    #[test]
    fn vendor_sighting_inherits_stable_identity_from_unique_airscan_match() {
        let scanners = scanners_from_sightings(
            vec![
                parse_scanimage_line("device 'brother4:net1;dev0' is 'Brother MFC-L2710DW'")
                    .unwrap(),
                parse_scanimage_line("device 'airscan:escl:http://BRW001122334455.local:80/eSCL/' is 'Brother MFC-L2710DW series'")
                    .unwrap(),
            ],
            InstalledState {
                brscan4: true,
                brscan5: false,
                brscan_skey: true,
            },
        )
        .unwrap();

        assert_eq!(scanners.len(), 1);
        assert_eq!(scanners[0].name, "MFC-L2710DW");
        assert_eq!(scanners[0].address, "brw001122334455.local:80");
        assert_eq!(scanners[0].id.as_str(), "brother_brw001122334455_2Elocal_3A80");
        let brother = scanners[0]
            .capabilities
            .extra
            .get("brother")
            .and_then(as_dict)
            .unwrap();
        assert_eq!(brother.get("driver"), Some(&Value::Str("brscan4".to_owned())));
        assert_eq!(brother.get("driver_installed"), Some(&Value::Bool(true)));
        assert_eq!(scanners[0].capabilities.buttons.count, 4);
    }

    #[test]
    fn unknown_models_keep_zero_buttons_and_no_driver_guess() {
        let scanners = scanners_from_sightings(
            vec![parse_scanimage_line(
                "device 'airscan:escl:http://192.168.1.23:80/eSCL/' is 'Brother Scanner XYZ'",
            )
            .unwrap()],
            InstalledState::default(),
        )
        .unwrap();

        assert_eq!(scanners.len(), 1);
        assert_eq!(scanners[0].capabilities.buttons.count, 0);
        let brother = scanners[0]
            .capabilities
            .extra
            .get("brother")
            .and_then(as_dict)
            .unwrap();
        assert!(!brother.contains_key("driver"));
    }

    #[test]
    fn url_host_and_usb_endpoint_are_extracted() {
        assert_eq!(
            physical_address_from_uri("http://BRW001122334455.local./eSCL"),
            Some("brw001122334455.local".to_owned())
        );
        assert_eq!(
            physical_address_from_uri("brother5:usb:001:002"),
            Some("usb:001:002".to_owned())
        );
    }
}
