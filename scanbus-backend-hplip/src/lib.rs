//! HP backend: discovery, install checks, and walk-up button listening through HPLIP.
//!
//! Network scanners are found by browsing the scanning service types ourselves (`mdns`),
//! because `hp-probe --bus=net` only ever looks for a printer; `hp-probe` covers the USB
//! bus, and the network bus only as a fallback. Package checks still come from the HPLIP
//! command-line tools.
//!
//! Walk-up button delivery comes from HPLIP's status service: `hp-systray
//! --force-startup` activates the canonical `com.hplip.StatusService` owner on the
//! session bus, and the resulting `hpssd` process emits `com.hplip.Toolbox.Event` updates
//! there while keeping the per-device history behind `GetHistory`. Scanbus may request
//! that canonical D-Bus activation, but it must not become a second process manager for
//! `hpssd.py`.
//!
//! # Requesting the activation is not enough on its own
//!
//! No HPLIP package ships a D-Bus service file for `com.hplip.StatusService`, so on a
//! stock system `StartServiceByName` answers `ServiceUnknown` and every HP pairing dies
//! before it can listen. The missing half lives in this repository's packaging, as
//! `packaging/dbus-1/services/scanbus-hplip-status.service`; the file is what makes the
//! bus able to run `hp-systray --force-startup` on demand, and the code here only asks.
//!
//! Because the activator is HPLIP's tray program, walk-up depends on `hplip-gui` (which
//! owns `/usr/bin/hp-systray`) and on the session having a `DISPLAY` — `hp-systray` exits
//! immediately without one. The first is checked up front in [`dependency_states`]; the
//! second can only be reported after the fact, which is what [`ActivationFailure`] is for.

mod mdns;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use futures_util::StreamExt as _;
// `mdns` unqualified is this crate's own HP-record filter, so the shared browse helper
// keeps its crate path at the one place it is called.
use scanbus_backend_common::{
    DEFAULT_SCANIMAGE_HELPER, ScanimageConfig, fetch_pages_via_scanimage,
};
use scanbus_core::{
    BackendError, ButtonsCapability, Capabilities, PairingProgress, ProfileKind, RawPage,
    ScanTrigger, ScannerBackend, ScannerId, ScannerInfo, Source, Status, Value,
};
use tokio::sync::mpsc;
use tracing::{debug, warn};
use zbus::fdo::DBusProxy;
use zbus::message::Type as MessageType;
use zbus::names::{BusName, WellKnownName};
use zbus::{DBusError as _, MatchRule, MessageStream};

/// Backend identifier, as it will be reported by [`ScannerBackend::id`].
pub const ID: &str = "hplip";

const DISCOVERY_ID: &str = "discovery";
const HPLIP_BUTTON_INDEX: u32 = 0;
const HPLIP_EVENT_SCAN_WAITING_FOR_PC: i32 = 2006;
const HPLIP_PACKAGE: &str = "hplip";
const HPAIO_PACKAGE: &str = "libsane-hpaio";
const HPLIP_GUI_PACKAGE: &str = "hplip-gui";
/// The activator named by our D-Bus service file; shipped by [`HPLIP_GUI_PACKAGE`].
const HP_SYSTRAY_PATH: &str = "/usr/bin/hp-systray";
/// Where [`crate`]'s packaging installs the activation file HPLIP itself does not ship.
const HPLIP_ACTIVATION_FILE: &str = "/usr/share/dbus-1/services/scanbus-hplip-status.service";
const PROPRIETARY_PLUGIN_PACKAGE: &str = "hp-plugin";
const HPLIP_SCAN_REASON_MASK: u32 = 0x40 | 0x80 | 0x100;
const HPLIP_STATUS_SERVICE: &str = "com.hplip.StatusService";
const HPLIP_STATUS_PATH: &str = "/com/hplip/StatusService";
const HPLIP_STATUS_INTERFACE: &str = "com.hplip.StatusService";
const HPLIP_TOOLBOX_INTERFACE: &str = "com.hplip.Toolbox";
/// eSCL, the service type a network MFP advertises its scanner on, and the one
/// `libhpdiscovery` browses first.
const USCAN_SERVICE_TYPE: &str = "_uscan._tcp.local.";
/// The older scanning service type, still all some HP devices publish.
const SCANNER_SERVICE_TYPE: &str = "_scanner._tcp.local.";
/// How long a discovery browse waits. Matched to `hp-probe`'s own `--timeout=5`, so
/// substituting the browse for the network probe does not lengthen a discovery.
const MDNS_BROWSE_WINDOW: Duration = Duration::from_secs(5);
/// `hp-probe` buses when the browse found the network side. USB probing is hpmud's own
/// job and has no mDNS equivalent.
const PROBE_BUSES_USB_ONLY: &str = "--bus=usb";
/// `hp-probe` buses when the browse found nothing: its SLP and JetDirect queries are the
/// fallback for devices that advertise no scanning service type.
const PROBE_BUSES_WITH_NET: &str = "--bus=net,usb";
const DBUS_SERVICE: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const DBUS_INTERFACE: &str = "org.freedesktop.DBus";

/// Walk-up HP backend, backed by an mDNS browse, `hp-probe` and HPLIP's model database.
#[derive(Debug, Clone)]
pub struct HplipBackend {
    hp_probe_path: PathBuf,
    dpkg_query_path: PathBuf,
    models_dat_path: PathBuf,
    scanimage_helper_path: PathBuf,
    fetch_state: Arc<Mutex<FetchState>>,
}

impl Default for HplipBackend {
    fn default() -> Self {
        Self {
            hp_probe_path: PathBuf::from("/usr/bin/hp-probe"),
            dpkg_query_path: PathBuf::from("/usr/bin/dpkg-query"),
            models_dat_path: PathBuf::from("/usr/share/hplip/data/models/models.dat"),
            scanimage_helper_path: PathBuf::from(DEFAULT_SCANIMAGE_HELPER),
            fetch_state: Arc::new(Mutex::new(FetchState::default())),
        }
    }
}

/// One discovered device, in both of the spellings HPLIP uses for it.
///
/// The two are not interchangeable and each has exactly one consumer: `hp:/…` is the
/// hpmud URI that the `hp-*` tools and `com.hplip.Toolbox.Event` speak, `hpaio:/…` is the
/// SANE device name that `scanimage --device-name` can open. Carrying only the first is
/// what makes a found scanner fail to open.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProbeRecord {
    device_uri: String,
    sane_name: String,
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

#[derive(Debug, Default)]
struct FetchState {
    active: Vec<ScannerId>,
    fetched: Vec<(ScannerId, String)>,
    /// The SANE device names, not the hpmud URIs: this map exists only to feed
    /// `scanimage --device-name`, which is the `hpaio:/…` spelling. See [`ProbeRecord`].
    sane_names: BTreeMap<ScannerId, String>,
}

type HplipHistoryEntry = (String, String, i32, String, i32, String, f64);

struct EventStream(mpsc::Receiver<ScanTrigger>);

struct FetchStream {
    inner: BoxStream<'static, Result<RawPage, BackendError>>,
    scanner: ScannerId,
    state: Arc<Mutex<FetchState>>,
}

impl futures_core::Stream for EventStream {
    type Item = ScanTrigger;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().0.poll_recv(cx)
    }
}

impl futures_core::Stream for FetchStream {
    type Item = Result<RawPage, BackendError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_next(cx)
    }
}

impl Drop for FetchStream {
    fn drop(&mut self) {
        let mut state = self
            .state
            .lock()
            .expect("hplip fetch state lock is not poisoned");
        state.active.retain(|scanner| scanner != &self.scanner);
    }
}

impl HplipBackend {
    fn discovery_scanner() -> ScannerId {
        ScannerId::from_backend(ID, DISCOVERY_ID).expect("static discovery id is valid")
    }

    fn discover_once(&self) -> Result<Vec<ScannerInfo>, BackendError> {
        let models = load_models(&self.models_dat_path).unwrap_or_else(|error| {
            warn!(
                path = %self.models_dat_path.display(),
                %error,
                "could not read HPLIP model metadata; discovery will continue without it"
            );
            BTreeMap::new()
        });

        let mut records = self.browse_network(&models);
        // `hp-probe` is kept for the USB bus, where hpmud probing is what finds a device
        // and has no mDNS equivalent, and for the network bus only when the browse came
        // back empty — the SLP and JetDirect queries behind `--bus=net` are how devices
        // that answer neither `_uscan._tcp` nor `_scanner._tcp` are still found.
        let buses = if records.is_empty() {
            PROBE_BUSES_WITH_NET
        } else {
            PROBE_BUSES_USB_ONLY
        };

        match self.probe(buses) {
            Ok(probed) => records.extend(probed),
            // A broken `hp-probe` must not bury what the browse already found: that
            // would put the USB bus back on the critical path for a network scanner,
            // which is the failure this whole path exists to undo.
            Err(error) if !records.is_empty() => {
                warn!(%error, "hp-probe failed; reporting the HP scanners found over mDNS");
            }
            Err(error) => return Err(error),
        }

        records
            .into_iter()
            .map(|record| scanner_from_probe(record, &models))
            .collect()
    }

    /// The HP scanners on the LAN, browsed from the scanning service types.
    ///
    /// Never an error, and empty on every failure: mDNS being unavailable says nothing
    /// about the USB bus, and the caller answers an empty browse with the `hp-probe`
    /// network fallback rather than with a failed discovery.
    fn browse_network(&self, models: &BTreeMap<String, ModelInfo>) -> Vec<ProbeRecord> {
        let services = match scanbus_backend_common::mdns::browse(
            &[USCAN_SERVICE_TYPE, SCANNER_SERVICE_TYPE],
            MDNS_BROWSE_WINDOW,
        ) {
            Ok(services) => services,
            Err(error) => {
                warn!(%error, "mDNS browse unavailable; falling back to hp-probe for the network bus");
                return Vec::new();
            }
        };

        let records = mdns::hp_scan_records(&services, models);
        debug!(
            answered = services.len(),
            hp_scanners = records.len(),
            "mDNS browse for HP network scanners finished"
        );
        records
    }

    /// What `hp-probe` reports on `buses`, or an empty list when HPLIP's tools are absent.
    fn probe(&self, buses: &str) -> Result<Vec<ProbeRecord>, BackendError> {
        if !self.hp_probe_path.exists() {
            warn!(
                path = %self.hp_probe_path.display(),
                "hp-probe is absent; treating HPLIP probing as empty"
            );
            return Ok(Vec::new());
        }

        let output = Command::new(&self.hp_probe_path)
            .args([
                buses,
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
                    "hp-probe {buses} failed with {}: {}",
                    output
                        .status
                        .code()
                        .map_or_else(|| "signal".to_owned(), |code| format!("exit code {code}")),
                    command_output(&output.stdout, &output.stderr)
                ),
            });
        }

        Ok(parse_probe_output(&String::from_utf8_lossy(&output.stdout)))
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

    /// One of the strings discovery published under `capabilities.extra["hplip"]`.
    fn published_name<'a>(scanner: &'a ScannerInfo, key: &str) -> Result<&'a str, BackendError> {
        scanner
            .capabilities
            .extra
            .get("hplip")
            .and_then(as_dict)
            .and_then(|dict| dict.get(key))
            .and_then(|value| match value {
                Value::Str(name) => Some(name.as_str()),
                _ => None,
            })
            .ok_or_else(|| {
                BackendError::Other(format!(
                    "scanner {} is missing `{key}` from HPLIP discovery",
                    scanner.id
                ))
            })
    }

    /// The hpmud URI, for talking to HPLIP itself — `hpssd`'s history, the `hp-*` tools.
    fn device_uri(scanner: &ScannerInfo) -> Result<&str, BackendError> {
        Self::published_name(scanner, "device_uri")
    }

    /// The SANE device name, for opening the scanner with `scanimage`.
    fn sane_name(scanner: &ScannerInfo) -> Result<&str, BackendError> {
        Self::published_name(scanner, "sane_name")
    }

    fn remember_sane_names(&self, scanners: &[ScannerInfo]) {
        let mut state = self
            .fetch_state
            .lock()
            .expect("hplip fetch state lock is not poisoned");
        state.sane_names.clear();
        for scanner in scanners {
            match Self::sane_name(scanner) {
                Ok(name) => {
                    state.sane_names.insert(scanner.id.clone(), name.to_owned());
                }
                Err(error) => {
                    warn!(scanner = %scanner.id, %error, "no SANE name for this HP; it will not be fetchable")
                }
            }
        }
    }

    async fn status_service_owner(
        scanner_id: &ScannerId,
        bus: &DBusProxy<'_>,
    ) -> Result<String, BackendError> {
        let service_name =
            WellKnownName::try_from(HPLIP_STATUS_SERVICE).expect("static bus name is valid");
        let bus_name = BusName::from(service_name.clone());
        let has_owner = bus.name_has_owner(bus_name).await.map_err(|error| {
            BackendError::Other(format!(
                "could not check HPLIP status service ownership: {error}"
            ))
        })?;
        if has_owner {
            return bus
                .get_name_owner(BusName::from(service_name.clone()))
                .await
                .map(|owner| owner.to_string())
                .map_err(|error| {
                    BackendError::Other(format!(
                        "could not resolve the HPLIP status service owner: {error}"
                    ))
                });
        }

        // Ask the session bus to start HPLIP's canonical owner. This keeps scanbus out of
        // the business of spawning `hpssd.py` directly while still avoiding a manual
        // pre-start requirement.
        let activation_error = bus
            .start_service_by_name(service_name.clone(), 0)
            .await
            .err();
        bus.get_name_owner(BusName::from(service_name))
            .await
            .map(|owner| owner.to_string())
            .map_err(|_| {
                BackendError::Other(hpssd_activation_missing_message(
                    scanner_id,
                    ActivationFailure::classify(activation_error.as_ref()),
                    activation_error
                        .as_ref()
                        .map(ToString::to_string)
                        .as_deref(),
                ))
            })
    }
}

#[async_trait]
impl ScannerBackend for HplipBackend {
    fn id(&self) -> &'static str {
        ID
    }

    async fn discover(&self) -> Result<Vec<ScannerInfo>, BackendError> {
        let backend = self.clone();
        let scanners = tokio::task::spawn_blocking(move || backend.discover_once())
            .await
            .map_err(|error| BackendError::Other(format!("hplip discover task failed: {error}")))?;
        let scanners = scanners?;
        self.remember_sane_names(&scanners);
        Ok(scanners)
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
        scanner: &ScannerInfo,
    ) -> Result<BoxStream<'static, ScanTrigger>, BackendError> {
        let device_uri = Self::device_uri(scanner)?.to_owned();
        let scanner_id = scanner.id.clone();
        let connection = zbus::Connection::session().await.map_err(|error| {
            BackendError::Other(format!("could not connect to the session bus: {error}"))
        })?;
        let bus = DBusProxy::new(&connection)
            .await
            .map_err(|error| BackendError::Other(format!("could not talk to D-Bus: {error}")))?;
        let owner = Self::status_service_owner(&scanner_id, &bus).await?;
        let status_proxy = zbus::Proxy::new(
            &connection,
            HPLIP_STATUS_SERVICE,
            HPLIP_STATUS_PATH,
            HPLIP_STATUS_INTERFACE,
        )
        .await
        .map_err(|error| BackendError::Other(format!("could not bind to hpssd: {error}")))?;

        let toolbox_rule = MatchRule::builder()
            .msg_type(MessageType::Signal)
            .sender(owner.as_str())
            .map_err(|error| BackendError::Other(format!("invalid hpssd owner name: {error}")))?
            .path("/")
            .map_err(|error| BackendError::Other(format!("invalid toolbox path: {error}")))?
            .interface(HPLIP_TOOLBOX_INTERFACE)
            .map_err(|error| BackendError::Other(format!("invalid toolbox interface: {error}")))?
            .member("Event")
            .map_err(|error| BackendError::Other(format!("invalid toolbox signal name: {error}")))?
            .build();
        let owner_rule = MatchRule::builder()
            .msg_type(MessageType::Signal)
            .sender(DBUS_SERVICE)
            .map_err(|error| BackendError::Other(format!("invalid D-Bus service name: {error}")))?
            .path(DBUS_PATH)
            .map_err(|error| BackendError::Other(format!("invalid D-Bus path: {error}")))?
            .interface(DBUS_INTERFACE)
            .map_err(|error| BackendError::Other(format!("invalid D-Bus interface: {error}")))?
            .member("NameOwnerChanged")
            .map_err(|error| {
                BackendError::Other(format!("invalid NameOwnerChanged member: {error}"))
            })?
            .add_arg(HPLIP_STATUS_SERVICE)
            .map_err(|error| {
                BackendError::Other(format!("invalid NameOwnerChanged filter: {error}"))
            })?
            .build();

        let mut toolbox_stream = MessageStream::for_match_rule(toolbox_rule, &connection, Some(8))
            .await
            .map_err(|error| {
                BackendError::Other(format!("could not subscribe to hpssd events: {error}"))
            })?;
        let mut owner_stream = MessageStream::for_match_rule(owner_rule, &connection, Some(4))
            .await
            .map_err(|error| {
                BackendError::Other(format!("could not watch hpssd ownership: {error}"))
            })?;

        let (sender, receiver) = mpsc::channel(8);
        tokio::spawn(async move {
            let mut last_trigger_id = None::<String>;

            loop {
                tokio::select! {
                    _ = sender.closed() => break,
                    message = owner_stream.next() => match message {
                        Some(Ok(message)) => {
                            if owner_lost(&message, owner.as_str()) {
                                debug!(scanner = %scanner_id, "hpssd left the session bus; ending the HP listener stream");
                                break;
                            }
                        }
                        Some(Err(error)) => {
                            warn!(scanner = %scanner_id, %error, "could not watch hpssd ownership");
                            break;
                        }
                        None => break,
                    },
                    message = toolbox_stream.next() => match message {
                        Some(Ok(message)) => {
                            let Ok((event_device_uri, _, _, _, _, _, _)) =
                                message.body().deserialize::<HplipHistoryEntry>() else {
                                continue;
                            };
                            if event_device_uri != device_uri {
                                continue;
                            }

                            let history = match status_proxy
                                .call::<_, _, (String, Vec<HplipHistoryEntry>)>("GetHistory", &(device_uri.as_str(),))
                                .await
                            {
                                Ok((_, history)) => history,
                                Err(error) => {
                                    warn!(scanner = %scanner_id, %error, "could not refresh hpssd history");
                                    break;
                                }
                            };

                            if let Some(trigger) = scan_waiting_for_pc_trigger(&scanner_id, &device_uri, &history) {
                                if last_trigger_id.as_deref() == Some(trigger.id.as_str()) {
                                    continue;
                                }
                                last_trigger_id = Some(trigger.id.clone());
                                if sender.send(trigger).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Some(Err(error)) => {
                            warn!(scanner = %scanner_id, %error, "the toolbox event stream failed");
                            break;
                        }
                        None => break,
                    },
                }
            }
        });

        Ok(Box::pin(EventStream(receiver)))
    }

    async fn stop_listening(&self, _scanner_id: &ScannerId) -> Result<(), BackendError> {
        Ok(())
    }

    async fn set_button_mapping(
        &self,
        scanner_id: &ScannerId,
        button_index: u32,
        _profile: Option<ProfileKind>,
        _options: &BTreeMap<String, Value>,
    ) -> Result<(), BackendError> {
        if !scanner_id.as_str().starts_with("hplip_") {
            return Err(BackendError::UnknownScanner(scanner_id.clone()));
        }
        if button_index != HPLIP_BUTTON_INDEX {
            return Err(BackendError::Other(format!(
                "scanner {scanner_id} exposes one generic HPLIP walk-up trigger, index {HPLIP_BUTTON_INDEX}; asked to map button {button_index}"
            )));
        }

        Ok(())
    }

    async fn fetch_pages(
        &self,
        scanner_id: &ScannerId,
        trigger_id: &str,
    ) -> Result<BoxStream<'static, Result<RawPage, BackendError>>, BackendError> {
        let fetch_key = (scanner_id.clone(), trigger_id.to_owned());
        {
            let mut state = self
                .fetch_state
                .lock()
                .expect("hplip fetch state lock is not poisoned");
            if state.fetched.contains(&fetch_key) {
                return Err(BackendError::UnknownJob {
                    scanner: scanner_id.clone(),
                    job: trigger_id.to_owned(),
                });
            }
            if state.active.contains(scanner_id) {
                return Err(BackendError::Busy(scanner_id.clone()));
            }
            state.active.push(scanner_id.clone());
        }

        // `scanimage --device-name` wants the SANE name; handing it the `hp:/…` URI is
        // what turns a scanner we did find into one we cannot open.
        let sane_name = {
            let state = self
                .fetch_state
                .lock()
                .expect("hplip fetch state lock is not poisoned");
            state.sane_names.get(scanner_id).cloned()
        };
        let Some(sane_name) = sane_name else {
            let mut state = self
                .fetch_state
                .lock()
                .expect("hplip fetch state lock is not poisoned");
            state.active.retain(|scanner| scanner != scanner_id);
            return Err(BackendError::UnknownScanner(scanner_id.clone()));
        };

        let mut config = ScanimageConfig::new(scanner_id.clone(), sane_name);
        config.program = self.scanimage_helper_path.clone();
        let pages = match fetch_pages_via_scanimage(config).await {
            Ok(pages) => pages,
            Err(error) => {
                let mut state = self
                    .fetch_state
                    .lock()
                    .expect("hplip fetch state lock is not poisoned");
                state.active.retain(|scanner| scanner != scanner_id);
                return Err(error);
            }
        };

        {
            let mut state = self
                .fetch_state
                .lock()
                .expect("hplip fetch state lock is not poisoned");
            state.fetched.push(fetch_key);
        }

        Ok(Box::pin(FetchStream {
            inner: pages,
            scanner: scanner_id.clone(),
            state: Arc::clone(&self.fetch_state),
        }))
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
            sane_name: sane_name_from_device_uri(device_uri),
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
    extra.insert("sane_name".to_owned(), Value::Str(record.sane_name.clone()));
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
            count: 1,
            label_configurable: false,
            labels: Vec::new(),
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

/// The SANE device name for an hpmud URI: `hp:/usb/…` becomes `hpaio:/usb/…`.
///
/// A prefix substitution and nothing more, because that is precisely what HPLIP itself
/// does — `base/device.py:1091` builds `scan-uri` as `device_uri.replace('hp:/',
/// 'hpaio:/')`, and `SCAN_URI`/`SANE_URI` at 1116 the same way.
///
/// In particular no `&queue=false` is appended here. That flag belongs to the mDNS path,
/// where [`mdns::hp_scan_record`] builds it in exactly as `libhpdiscovery` does; for a
/// URI that came out of `hp-probe`, `libsane-hpaio.so.1` composes the hpmud URI itself
/// (it carries both `hp:/net/` and `&queue=false` as literals), and second-guessing it
/// would only produce a name HPLIP never emits.
fn sane_name_from_device_uri(device_uri: &str) -> String {
    device_uri
        .strip_prefix("hp:/")
        .map_or_else(|| device_uri.to_owned(), |rest| format!("hpaio:/{rest}"))
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
        // Not a cosmetic extra: `hp-systray` lives in `hplip-gui`, and it is the only
        // thing that ever claims `com.hplip.StatusService`. Without this package the
        // activation in `status_service_owner` cannot succeed however it is invoked, so
        // checking it here turns a puzzling "no owner appeared" at listen time into a
        // named missing package at the start of pairing.
        DependencyState {
            package: HPLIP_GUI_PACKAGE,
            installed: package_installed(
                dpkg_query_path,
                HPLIP_GUI_PACKAGE,
                &[Path::new(HP_SYSTRAY_PATH)],
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

fn scan_waiting_for_pc_trigger(
    scanner_id: &ScannerId,
    device_uri: &str,
    history: &[HplipHistoryEntry],
) -> Option<ScanTrigger> {
    let (event_device_uri, _, event_code, _, job_id, _, timedate) = history.last()?;
    if event_device_uri != device_uri || *event_code != HPLIP_EVENT_SCAN_WAITING_FOR_PC {
        return None;
    }

    let millis = (*timedate * 1000.0).round();
    if !millis.is_finite() || millis < 0.0 {
        return None;
    }

    Some(ScanTrigger {
        id: format!("hpssd:{event_code}:{job_id}:{}", millis as u64),
        scanner_id: scanner_id.clone(),
        kind: scanbus_core::TriggerKind::Button {
            index: HPLIP_BUTTON_INDEX,
        },
        timestamp: UNIX_EPOCH + Duration::from_millis(millis as u64),
    })
}

fn owner_lost(message: &zbus::Message, previous_owner: &str) -> bool {
    let Ok((_, old_owner, new_owner)) = message.body().deserialize::<(String, String, String)>()
    else {
        return false;
    };
    old_owner == previous_owner && new_owner.is_empty()
}

/// Why the session bus produced no owner for `com.hplip.StatusService`.
///
/// The three cases need three different fixes and only the D-Bus reply tells them apart,
/// so the failure is classified before it is rendered: an error message naming the wrong
/// remedy is what sent issue #64 looking for a missing HPLIP feature when the missing
/// piece was our own activation file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivationFailure {
    /// `ServiceUnknown` — no `.service` file on the bus declares the name at all.
    NoServiceFile,
    /// `Spawn.*` — the bus tried to run the activator and could not.
    Spawn,
    /// Activation reported success, yet nobody holds the name: `hp-systray` gave up.
    NoOwner,
}

impl ActivationFailure {
    fn classify(error: Option<&zbus::fdo::Error>) -> Self {
        match error {
            None => Self::NoOwner,
            Some(zbus::fdo::Error::ServiceUnknown(_)) => Self::NoServiceFile,
            Some(other) => {
                if other
                    .name()
                    .starts_with("org.freedesktop.DBus.Error.Spawn.")
                {
                    Self::Spawn
                } else {
                    Self::NoOwner
                }
            }
        }
    }

    /// The one thing worth doing about this failure, in the operator's terms.
    fn remedy(self) -> String {
        match self {
            Self::NoServiceFile => format!(
                "no D-Bus activation file on this session declares com.hplip.StatusService — \
HPLIP ships none, so scanbus provides one at {HPLIP_ACTIVATION_FILE}; install the scanbus \
backend package (or run `make install-services`) and reload the session bus with `make reload`"
            ),
            Self::Spawn => format!(
                "the session bus could not run the activator; install the hplip-gui package so \
that {HP_SYSTRAY_PATH} exists and is executable"
            ),
            Self::NoOwner => "hp-systray ran but never claimed the name; it refuses to start \
without DISPLAY, so check the daemon's session environment with \
`systemctl --user show-environment`"
                .to_owned(),
        }
    }
}

fn hpssd_activation_missing_message(
    scanner_id: &ScannerId,
    failure: ActivationFailure,
    activation_error: Option<&str>,
) -> String {
    let activation_detail = activation_error.map_or_else(String::new, |error| {
        format!(" (activation returned: {error})")
    });
    let remedy = failure.remedy();
    format!(
        "com.hplip.StatusService is not running on the session bus for scanner {scanner_id}; \
scanbus asked D-Bus to activate HPLIP's canonical owner via hp-systray --force-startup, \
but no owner appeared{activation_detail}; {remedy}; scanbus will not start hpssd.py directly"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::SystemTime;

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
                sane_name: "hpaio:/net/officejet_pro_9010?ip=192.168.1.9".to_owned(),
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
                sane_name: "hpaio:/usb/officejet_8710_series?serial=CN1234ABCDE".to_owned(),
                model_name: "HP OfficeJet 8710 series".to_owned(),
                display_name: "HP OfficeJet 8710 series".to_owned(),
            }]
        );
    }

    /// `scanimage` cannot open an `hp:/…` URI — that spelling is hpmud's. Discovery
    /// therefore publishes both names, or fixing discovery only moves the failure from
    /// "not found" to "cannot open".
    #[test]
    fn both_spellings_are_published_for_a_discovered_device() {
        let record = ProbeRecord {
            device_uri: "hp:/net/HP_OfficeJet_Pro_8610?ip=192.168.1.3&queue=false".to_owned(),
            sane_name: "hpaio:/net/HP_OfficeJet_Pro_8610?ip=192.168.1.3&queue=false".to_owned(),
            model_name: "HP OfficeJet Pro 8610".to_owned(),
            display_name: "HP OfficeJet Pro 8610".to_owned(),
        };
        let capabilities = capabilities_from_model(
            &record,
            &Some("hp_officejet_pro_8610".to_owned()),
            &ModelInfo::default(),
        );
        let hplip = capabilities
            .extra
            .get("hplip")
            .and_then(as_dict)
            .expect("discovery publishes an hplip section");

        assert_eq!(
            hplip.get("device_uri"),
            Some(&Value::Str(record.device_uri.clone()))
        );
        assert_eq!(
            hplip.get("sane_name"),
            Some(&Value::Str(record.sane_name.clone()))
        );
    }

    /// HPLIP's own rule, `base/device.py:1091`: substitute the scheme, change nothing
    /// else — the query string, `queue=false` included, is carried across untouched.
    #[test]
    fn the_sane_name_is_the_hpmud_uri_with_its_scheme_substituted() {
        assert_eq!(
            sane_name_from_device_uri("hp:/net/HP_OfficeJet_Pro_8610?ip=192.168.1.3&queue=false"),
            "hpaio:/net/HP_OfficeJet_Pro_8610?ip=192.168.1.3&queue=false"
        );
        assert_eq!(
            sane_name_from_device_uri("hp:/usb/officejet_8710_series?serial=CN1234ABCDE"),
            "hpaio:/usb/officejet_8710_series?serial=CN1234ABCDE"
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

    /// The URI shape the mDNS path builds. `queue=false` is a field this function does
    /// not know and must ignore rather than refuse, and `ip=` stays ahead of `hostname=`:
    /// an mDNS resolution yields a `.local.` name *as well as* addresses, and publishing
    /// the name would leave the daemon unable to match this sighting with the same
    /// device seen over `escl`.
    #[test]
    fn an_mdns_uri_is_addressed_by_its_ipv4() {
        assert_eq!(
            physical_address_from_uri("hp:/net/HP_OfficeJet_Pro_8610?ip=192.168.1.3&queue=false"),
            Some("192.168.1.3".to_owned())
        );
        assert_eq!(
            physical_address_from_uri(
                "hp:/net/HP_OfficeJet_Pro_8610?ip=192.168.1.3&hostname=HP8610.local.&queue=false"
            ),
            Some("192.168.1.3".to_owned())
        );
    }

    #[test]
    fn capabilities_follow_scan_src_and_plugin_flags() {
        let record = ProbeRecord {
            device_uri: "hp:/net/officejet_pro_9010?ip=192.168.1.9".to_owned(),
            sane_name: "hpaio:/net/officejet_pro_9010?ip=192.168.1.9".to_owned(),
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
        assert_eq!(capabilities.buttons.count, 1);
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

    #[test]
    fn scan_waiting_for_pc_history_becomes_button_zero() {
        let scanner_id = ScannerId::from_backend(ID, "192.168.1.9").unwrap();
        let history = vec![(
            "hp:/net/officejet_pro_9010?ip=192.168.1.9".to_owned(),
            String::new(),
            HPLIP_EVENT_SCAN_WAITING_FOR_PC,
            "jean".to_owned(),
            0,
            String::new(),
            1_723_472_000.125,
        )];

        let trigger = scan_waiting_for_pc_trigger(
            &scanner_id,
            "hp:/net/officejet_pro_9010?ip=192.168.1.9",
            &history,
        )
        .expect("the waiting-for-pc event should become a trigger");

        assert_eq!(trigger.scanner_id, scanner_id);
        assert_eq!(trigger.kind, scanbus_core::TriggerKind::Button { index: 0 });
        assert_eq!(trigger.id, "hpssd:2006:0:1723472000125");
        assert_eq!(
            trigger.timestamp,
            SystemTime::UNIX_EPOCH + Duration::from_millis(1_723_472_000_125)
        );
    }

    #[test]
    fn only_the_latest_waiting_for_pc_event_triggers() {
        let scanner_id = ScannerId::from_backend(ID, "192.168.1.9").unwrap();
        let history = vec![
            (
                "hp:/net/officejet_pro_9010?ip=192.168.1.9".to_owned(),
                String::new(),
                HPLIP_EVENT_SCAN_WAITING_FOR_PC,
                "jean".to_owned(),
                0,
                String::new(),
                1.0,
            ),
            (
                "hp:/net/officejet_pro_9010?ip=192.168.1.9".to_owned(),
                String::new(),
                2000,
                "jean".to_owned(),
                0,
                String::new(),
                2.0,
            ),
        ];

        assert!(
            scan_waiting_for_pc_trigger(
                &scanner_id,
                "hp:/net/officejet_pro_9010?ip=192.168.1.9",
                &history
            )
            .is_none()
        );
    }

    #[test]
    fn hpssd_activation_message_mentions_dbus_activation_and_no_direct_spawn() {
        let scanner_id = ScannerId::from_backend(ID, "192.168.1.9").unwrap();
        let message =
            hpssd_activation_missing_message(&scanner_id, ActivationFailure::NoOwner, None);

        assert!(
            message.contains("activate HPLIP's canonical owner via hp-systray --force-startup")
        );
        assert!(message.contains("will not start hpssd.py directly"));
    }

    #[test]
    fn hpssd_activation_message_includes_activation_failure_details() {
        let scanner_id = ScannerId::from_backend(ID, "192.168.1.9").unwrap();
        let message = hpssd_activation_missing_message(
            &scanner_id,
            ActivationFailure::NoServiceFile,
            Some("org.freedesktop.DBus.Error.ServiceUnknown"),
        );

        assert!(message.contains("activation returned: org.freedesktop.DBus.Error.ServiceUnknown"));
    }

    /// The three failures are told apart by the D-Bus reply, and only by it: issue #64
    /// read as "HPLIP is broken" when what was missing was the activation file scanbus
    /// itself ships.
    #[test]
    fn activation_failures_are_classified_from_the_dbus_reply() {
        assert_eq!(
            ActivationFailure::classify(Some(&zbus::fdo::Error::ServiceUnknown(
                "The name com.hplip.StatusService was not provided by any .service files"
                    .to_owned()
            ))),
            ActivationFailure::NoServiceFile
        );
        assert_eq!(
            ActivationFailure::classify(Some(&zbus::fdo::Error::SpawnExecFailed(
                "no such file".to_owned()
            ))),
            ActivationFailure::Spawn
        );
        assert_eq!(
            ActivationFailure::classify(None),
            ActivationFailure::NoOwner
        );
    }

    #[test]
    fn each_activation_failure_names_its_own_remedy() {
        let scanner_id = ScannerId::from_backend(ID, "192.168.1.9").unwrap();

        let missing_file =
            hpssd_activation_missing_message(&scanner_id, ActivationFailure::NoServiceFile, None);
        assert!(missing_file.contains("scanbus-hplip-status.service"));

        let spawn = hpssd_activation_missing_message(&scanner_id, ActivationFailure::Spawn, None);
        assert!(spawn.contains(HP_SYSTRAY_PATH));

        let no_owner =
            hpssd_activation_missing_message(&scanner_id, ActivationFailure::NoOwner, None);
        assert!(no_owner.contains("DISPLAY"));
    }

    /// `hplip-gui` owns `/usr/bin/hp-systray`, the only process that ever claims
    /// `com.hplip.StatusService`. Pairing has to name it as a missing package rather than
    /// let listening fail later with a bus error nobody can act on.
    #[test]
    fn pairing_reports_a_missing_hplip_gui_by_name() {
        let tempdir = tempfile::tempdir().unwrap();
        let dpkg_query = tempdir.path().join("dpkg-query");
        fs::write(
            &dpkg_query,
            "#!/bin/sh\n\
for package; do :; done\n\
if [ \"$package\" = \"hplip-gui\" ]; then exit 1; fi\n\
printf 'installed\\n'\n",
        )
        .unwrap();
        let mut perms = fs::metadata(&dpkg_query).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&dpkg_query, perms).unwrap();

        let backend = HplipBackend {
            dpkg_query_path: dpkg_query,
            ..HplipBackend::default()
        };
        let record = ProbeRecord {
            device_uri: "hp:/net/officejet_pro_9010?ip=192.168.1.9".to_owned(),
            sane_name: "hpaio:/net/officejet_pro_9010?ip=192.168.1.9".to_owned(),
            model_name: "HP OfficeJet Pro 9010".to_owned(),
            display_name: "OfficeJet Pro 9010".to_owned(),
        };
        let scanner = scanner_from_probe(record, &BTreeMap::new()).unwrap();

        let error = backend.ensure_installed_once(&scanner).unwrap_err();
        assert_eq!(
            error,
            BackendError::InstallFailed {
                package: HPLIP_GUI_PACKAGE.to_owned(),
                detail: format!(
                    "{HPLIP_GUI_PACKAGE} is not installed; install it with your distribution package manager"
                ),
            }
        );
    }

    #[tokio::test]
    async fn set_button_mapping_accepts_the_single_generic_hplip_button() {
        let backend = HplipBackend::default();
        let scanner_id = ScannerId::from_backend(ID, "192.168.1.9").unwrap();

        backend
            .set_button_mapping(
                &scanner_id,
                0,
                Some(ProfileKind::Document),
                &BTreeMap::new(),
            )
            .await
            .unwrap();

        let error = backend
            .set_button_mapping(
                &scanner_id,
                1,
                Some(ProfileKind::Document),
                &BTreeMap::new(),
            )
            .await
            .unwrap_err();
        // The variant, not only the wording: an index off the table is `Failed`, the
        // unclassified error, because the scanner is known and the operation supported.
        // Brother's four-entry panel refuses the same mistake the same way (5.11).
        assert!(
            matches!(&error, BackendError::Other(message)
                if message.contains("one generic HPLIP walk-up trigger")),
            "{error:?}",
        );
    }

    #[tokio::test]
    async fn fetch_pages_is_busy_while_a_scan_is_running_and_unknown_afterwards() {
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
sleep 0.2\n\
printf 'P5\\n1 1\\n255\\n\\003' > \"$(printf \"$batch\" 1)\"\n",
        )
        .unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).unwrap();

        let backend = HplipBackend {
            scanimage_helper_path: script,
            ..HplipBackend::default()
        };
        let scanner_id = ScannerId::from_backend(ID, "192.168.1.9").unwrap();
        backend.fetch_state.lock().unwrap().sane_names.insert(
            scanner_id.clone(),
            "hpaio:/net/officejet_pro_9010?ip=192.168.1.9".to_owned(),
        );

        let mut stream = backend.fetch_pages(&scanner_id, "job-1").await.unwrap();
        let busy = match backend.fetch_pages(&scanner_id, "job-2").await {
            Ok(_) => panic!("a concurrent fetch on the same scanner should be busy"),
            Err(error) => error,
        };
        assert_eq!(busy, BackendError::Busy(scanner_id.clone()));

        let page = stream.next().await.unwrap().unwrap();
        assert_eq!(page.index, 0);
        assert!(stream.next().await.is_none());

        let unknown = match backend.fetch_pages(&scanner_id, "job-1").await {
            Ok(_) => panic!("the same trigger id must not be fetchable twice"),
            Err(error) => error,
        };
        assert_eq!(
            unknown,
            BackendError::UnknownJob {
                scanner: scanner_id,
                job: "job-1".to_owned(),
            }
        );
    }

    /// The two spellings are not interchangeable at the point of use: `scanimage` only
    /// knows `hpaio:/…`, so the map `fetch_pages` reads has to hold that one, while
    /// `start_listening` keeps matching `hpssd` history entries against `hp:/…`.
    #[test]
    fn discovery_remembers_the_sane_name_and_leaves_the_hpmud_uri_to_listening() {
        let backend = HplipBackend::default();
        let record = ProbeRecord {
            device_uri: "hp:/net/officejet_pro_9010?ip=192.168.1.9".to_owned(),
            sane_name: "hpaio:/net/officejet_pro_9010?ip=192.168.1.9".to_owned(),
            model_name: "HP OfficeJet Pro 9010".to_owned(),
            display_name: "OfficeJet Pro 9010".to_owned(),
        };
        let scanner = scanner_from_probe(record.clone(), &BTreeMap::new()).unwrap();

        assert_eq!(
            HplipBackend::device_uri(&scanner).unwrap(),
            record.device_uri
        );
        assert_eq!(HplipBackend::sane_name(&scanner).unwrap(), record.sane_name);

        backend.remember_sane_names(std::slice::from_ref(&scanner));
        assert_eq!(
            backend
                .fetch_state
                .lock()
                .unwrap()
                .sane_names
                .get(&scanner.id),
            Some(&record.sane_name)
        );
    }

    /// The whole point of carrying two names: what reaches `scanimage` is the openable one.
    #[tokio::test]
    async fn a_fetch_opens_the_scanner_by_its_sane_name() {
        let tempdir = tempfile::tempdir().unwrap();
        let argv = tempdir.path().join("argv");
        let script = tempdir.path().join("scanimage.sh");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
batch=\n\
for arg in \"$@\"; do\n\
  printf '%s\\n' \"$arg\" >> {argv}\n\
  case \"$arg\" in\n\
    --batch=*) batch=${{arg#--batch=}} ;;\n\
  esac\n\
done\n\
printf 'P5\\n1 1\\n255\\n\\003' > \"$(printf \"$batch\" 1)\"\n",
                argv = argv.display()
            ),
        )
        .unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).unwrap();

        let backend = HplipBackend {
            scanimage_helper_path: script,
            ..HplipBackend::default()
        };
        let record = ProbeRecord {
            device_uri: "hp:/net/HP_OfficeJet_Pro_8610?ip=192.168.1.3&queue=false".to_owned(),
            sane_name: "hpaio:/net/HP_OfficeJet_Pro_8610?ip=192.168.1.3&queue=false".to_owned(),
            model_name: "HP OfficeJet Pro 8610".to_owned(),
            display_name: "HP OfficeJet Pro 8610".to_owned(),
        };
        let scanner = scanner_from_probe(record.clone(), &BTreeMap::new()).unwrap();
        backend.remember_sane_names(std::slice::from_ref(&scanner));

        let mut stream = backend.fetch_pages(&scanner.id, "job-1").await.unwrap();
        while stream.next().await.is_some() {}

        let arguments = fs::read_to_string(&argv).unwrap();
        assert!(
            arguments
                .lines()
                .any(|argument| argument == format!("--device-name={}", record.sane_name)),
            "scanimage was called with {arguments:?}"
        );
    }
}
