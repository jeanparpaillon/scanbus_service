//! Brother backend: discovery plus install checks for `brscan4`/`brscan5`.
//!
//! The first implementation intentionally starts from `scanimage -L`: on the target
//! machine it already enumerates Brother devices through the proprietary SANE backend
//! as well as any eSCL-capable ones through airscan, which gives us one probe to parse
//! instead of a fresh SNMP/mDNS implementation before the basics are tested.
//!
//! # This backend never installs anything
//!
//! `brscan4`/`brscan5` and `brscan-skey` are `.deb` files from Brother's website,
//! outside any apt repository ([`scanbus-rust-implementation.md`] §4). The obvious
//! reading of that — download the `.deb` and hand it to `dpkg -i` through `pkexec` —
//! is deliberately not implemented: it would make every scanbus install a supply-chain
//! decision the user was never shown, and would turn a session daemon into a
//! privilege-escalation surface reachable from the session bus by any process.
//!
//! [`ensure_installed`](BrotherBackend::ensure_installed) therefore *verifies, or
//! refuses*: it reports what is missing, by name and by where it comes from, and stops.
//! Assisted installation is a later, separate step, and needs pieces this backend does
//! not have — pinned URLs and checksums shipped in the package rather than fetched, an
//! explicit confirmation through a UI, and a policy file the distribution can audit.
//!
//! Two things enforce that in code rather than in prose: every subprocess goes through
//! [`CommandRunner`], so a test can assert the exact set of programs this crate is able
//! to run, and `tests::the_backend_has_no_way_to_install_anything` greps the non-test
//! source for the mechanisms that would be needed to install something.
//!
//! **The invariant is "installs nothing", not "no network".** Speaking the Brother
//! push-button protocol ourselves ([`skey`]) means an SNMP exchange on UDP/161 and a
//! listener on UDP/54925 — and that is the whole point of
//! [`brother-skeyless-backend.md`]: a datagram sent to a printer on the local network is
//! not a package fetched from a vendor's website, and doing it ourselves is what removes
//! the two `.deb` files from the story rather than adding to it. What the guard test
//! forbids is a package manager, an HTTP client, a downloader, and privilege escalation.
//!
//! [`scanbus-rust-implementation.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-rust-implementation.md
//! [`brother-skeyless-backend.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/brother-skeyless-backend.md

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use scanbus_core::{
    BackendError, ButtonsCapability, Capabilities, ColorMode, PairingProgress, ProfileKind,
    RawPage, RestoreDisposition, ScanTrigger, ScannerBackend, ScannerId, ScannerInfo, Source,
    Status, Value,
};
use tokio::sync::mpsc;
use tracing::warn;

pub mod skey;

/// Backend identifier, as it will be reported by [`ScannerBackend::id`].
pub const ID: &str = "brother-skey";

const SCANNER_ID_BACKEND: &str = "brother";
const SCANNER_BACKEND_NAME: &str = "proprietary:brother";

/// Where the packages come from, since no apt repository carries them.
///
/// Named in every "not installed" message: a user told only "brscan4 is missing" has
/// nowhere to go, because `apt install brscan4` cannot work on any distribution.
const BROTHER_SUPPORT_SITE: &str = "https://support.brother.com/";

/// Everything the backend needs to decide whether one package is usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PackageProbe {
    package: &'static str,
    /// Absolute paths that only exist when the package's files are on disk.
    ///
    /// Checked before any subprocess: on a machine where the driver is present this is
    /// the whole check, and it costs no fork.
    files: &'static [&'static str],
}

const BRSCAN4_PROBE: PackageProbe = PackageProbe {
    package: "brscan4",
    files: &[
        "/opt/brother/scanner/brscan4",
        "/usr/lib64/sane/libsane-brother4.so",
        "/usr/lib/x86_64-linux-gnu/sane/libsane-brother4.so",
    ],
};

const BRSCAN5_PROBE: PackageProbe = PackageProbe {
    package: "brscan5",
    files: &[
        "/opt/brother/scanner/brscan5",
        "/usr/lib64/sane/libsane-brother5.so",
        "/usr/lib/x86_64-linux-gnu/sane/libsane-brother5.so",
    ],
};

const BRSCAN_SKEY_PROBE: PackageProbe = PackageProbe {
    package: "brscan-skey",
    files: &["/usr/bin/brscan-skey", "/opt/brother/scanner/brscan-skey"],
};

/// The seam every subprocess in this crate goes through.
///
/// It exists so that "this backend cannot install anything" is a property a test can
/// check rather than a claim in a comment: a stub runner records every invocation, and
/// the allowlist test asserts the programs are exactly `scanimage` and `dpkg-query`,
/// both read-only.
trait CommandRunner: fmt::Debug + Send + Sync {
    fn run(&self, program: &Path, args: &[&str]) -> io::Result<Output>;
}

/// The real one: spawns the program, and is the only `Command::new` in the crate.
#[derive(Debug, Clone, Copy)]
struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, program: &Path, args: &[&str]) -> io::Result<Output> {
        Command::new(program).args(args).output()
    }
}

/// Walk-up Brother backend backed by `scanimage -L` plus local package checks.
#[derive(Debug, Clone)]
pub struct BrotherBackend {
    scanimage_path: PathBuf,
    dpkg_query_path: PathBuf,
    /// Prefix every probed path is resolved against. `/` outside tests; a tempdir in
    /// them, which is what lets a test mask `brscan4` without touching the machine.
    sysroot: PathBuf,
    runner: Arc<dyn CommandRunner>,
}

impl Default for BrotherBackend {
    fn default() -> Self {
        Self {
            scanimage_path: PathBuf::from("/usr/bin/scanimage"),
            dpkg_query_path: PathBuf::from("/usr/bin/dpkg-query"),
            sysroot: PathBuf::from("/"),
            runner: Arc::new(SystemCommandRunner),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Driver {
    Brscan4,
    Brscan5,
}

impl Driver {
    const fn probe(self) -> PackageProbe {
        match self {
            Self::Brscan4 => BRSCAN4_PROBE,
            Self::Brscan5 => BRSCAN5_PROBE,
        }
    }

    const fn package(self) -> &'static str {
        self.probe().package
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

/// What the two checks together say about one package.
///
/// The middle variant is the reason there are two checks. `dpkg-query` answering
/// `installed` is not proof that anything is on disk — a `.deb` unpacked by hand, a
/// `/opt` wiped by a cleanup script and an aborted upgrade all leave the status
/// database saying yes over an empty directory. Pairing on that answer fails later, in
/// `start_listening`, with an error about SANE rather than about the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Presence {
    /// Files are on disk. The driver can be used, whatever the packaging system thinks
    /// — including on a machine with no `dpkg` at all.
    Present,
    /// The packaging system has the package registered as installed, and none of its
    /// files are there. Reinstalling is the fix, so the message has to say so.
    RegisteredButMissing,
    /// Neither check found anything.
    #[default]
    Absent,
}

impl Presence {
    const fn is_usable(self) -> bool {
        matches!(self, Self::Present)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct InstalledState {
    brscan4: Presence,
    brscan5: Presence,
    brscan_skey: Presence,
}

impl InstalledState {
    const fn driver_installed(self, driver: Driver) -> bool {
        match driver {
            Driver::Brscan4 => self.brscan4.is_usable(),
            Driver::Brscan5 => self.brscan5.is_usable(),
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

        let output = self
            .runner
            .run(&self.scanimage_path, &["-L"])
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

        let installed = self.installed_state()?;
        scanners_from_sightings(
            parse_scanimage_output(&String::from_utf8_lossy(&output.stdout)),
            installed,
        )
    }

    /// Which driver this scanner needs, from what discovery recorded about it.
    fn required_driver_for(scanner: &ScannerInfo) -> Result<Driver, BackendError> {
        let metadata = brother_metadata(scanner).ok_or_else(|| BackendError::InstallFailed {
            package: "brother-driver".to_owned(),
            detail: format!(
                "scanner {} is missing Brother discovery metadata; rediscover it first",
                scanner.id
            ),
        })?;
        metadata.driver.ok_or_else(|| BackendError::InstallFailed {
            package: "brother-driver".to_owned(),
            detail: format!(
                "scanbus does not know whether model {} requires brscan4 or brscan5 yet",
                scanner.name
            ),
        })
    }

    /// One dependency step: announce it, check it, and refuse if it is not usable.
    ///
    /// The [`PairingProgress::Installing`] goes out *before* the check, not after it.
    /// Nothing is being installed — but `installing_backend` is the state this phase of
    /// pairing is in ([`scanbus-rust-implementation.md`] §5), and a client whose
    /// progress UI only ever saw `pairing` → `done` here would have to be rewritten the
    /// day assisted installation lands. The observable sequence is the contract; what
    /// happens inside the step is not.
    ///
    /// [`scanbus-rust-implementation.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-rust-implementation.md
    async fn check_dependency(
        &self,
        probe: PackageProbe,
        progress: &mpsc::Sender<PairingProgress>,
    ) -> Result<(), BackendError> {
        let _ = progress
            .send(PairingProgress::Installing {
                package: probe.package.to_owned(),
                percent: None,
            })
            .await;

        let backend = self.clone();
        let presence = tokio::task::spawn_blocking(move || backend.package_presence(probe))
            .await
            .map_err(|error| {
                BackendError::Other(format!("brother dependency check failed: {error}"))
            })??;

        if presence.is_usable() {
            return Ok(());
        }

        Err(BackendError::InstallFailed {
            package: probe.package.to_owned(),
            detail: missing_package_detail(probe, presence),
        })
    }

    /// The driver this model needs, then `brscan-skey`, each its own progress step.
    ///
    /// Two steps rather than one because they fail for different reasons and are fixed
    /// by installing different packages: a machine can have `brscan4` from an earlier
    /// SANE-only setup and no `brscan-skey` at all, which is exactly the case where
    /// scanning works and walk-up keys do not.
    async fn check_dependencies(
        &self,
        scanner: &ScannerInfo,
        progress: &mpsc::Sender<PairingProgress>,
    ) -> Result<(), BackendError> {
        let driver = Self::required_driver_for(scanner)?;
        self.check_dependency(driver.probe(), progress).await?;
        self.check_dependency(BRSCAN_SKEY_PROBE, progress).await
    }

    fn installed_state(&self) -> Result<InstalledState, BackendError> {
        Ok(InstalledState {
            brscan4: self.package_presence(BRSCAN4_PROBE)?,
            brscan5: self.package_presence(BRSCAN5_PROBE)?,
            brscan_skey: self.package_presence(BRSCAN_SKEY_PROBE)?,
        })
    }

    /// Files first, packaging system second — and the second only when the first found
    /// nothing.
    ///
    /// The order is what keeps the check honest on a machine the packaging system does
    /// not describe: a driver whose files are there is usable even where `dpkg-query`
    /// is absent or knows nothing, and the query is only needed to tell "never
    /// installed" apart from "installed, then gutted". `dpkg-query -L` is asked for the
    /// package's own file list rather than trusting [`PackageProbe::files`], because
    /// the SANE library path is architecture-dependent and a hard-coded
    /// `x86_64-linux-gnu` would report a working aarch64 install as broken.
    fn package_presence(&self, probe: PackageProbe) -> Result<Presence, BackendError> {
        if probe.files.iter().any(|file| self.resolve(file).exists()) {
            return Ok(Presence::Present);
        }

        if !self.dpkg_query_path.exists() {
            return Ok(Presence::Absent);
        }

        if !self.package_registered(probe.package)? {
            return Ok(Presence::Absent);
        }

        if self
            .package_files(probe.package)?
            .iter()
            .any(|file| self.resolve(file).is_file())
        {
            return Ok(Presence::Present);
        }

        Ok(Presence::RegisteredButMissing)
    }

    /// `dpkg-query -W`: does the packaging system consider this package installed?
    fn package_registered(&self, package: &str) -> Result<bool, BackendError> {
        let output = self
            .runner
            .run(
                &self.dpkg_query_path,
                &["-W", "-f=${db:Status-Status}\\n", package],
            )
            .map_err(|error| {
                BackendError::Other(format!("failed to run dpkg-query for {package}: {error}"))
            })?;

        // A package the database has never heard of is an error exit, not an answer.
        Ok(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).trim() == "installed",
        )
    }

    /// `dpkg-query -L`: the paths the package claims to own.
    fn package_files(&self, package: &str) -> Result<Vec<String>, BackendError> {
        let output = self
            .runner
            .run(&self.dpkg_query_path, &["-L", package])
            .map_err(|error| {
                BackendError::Other(format!(
                    "failed to list the files of {package} with dpkg-query: {error}"
                ))
            })?;

        if !output.status.success() {
            return Ok(Vec::new());
        }

        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with('/'))
            .map(str::to_owned)
            .collect())
    }

    fn resolve(&self, absolute: &str) -> PathBuf {
        self.sysroot.join(absolute.trim_start_matches('/'))
    }
}

/// What a client is told about a package it has to install itself.
///
/// Names the package and where it comes from, because neither is guessable: `apt
/// install brscan4` works on no distribution, and the two failures need different
/// actions from the user.
fn missing_package_detail(probe: PackageProbe, presence: Presence) -> String {
    let package = probe.package;
    match presence {
        Presence::Present => format!("{package} is installed"),
        Presence::RegisteredButMissing => format!(
            "{package} is registered as installed but none of its files are on disk; \
             reinstall the package from {BROTHER_SUPPORT_SITE} — scanbus does not install it"
        ),
        Presence::Absent => format!(
            "{package} is not installed and is in no distribution repository; \
             download it from {BROTHER_SUPPORT_SITE} and install it, then pair again — \
             scanbus does not install it"
        ),
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
            .map_err(|error| {
                BackendError::Other(format!("brother discover task failed: {error}"))
            })?
    }

    /// Verifies the Brother dependencies, or refuses. Never installs anything — see the
    /// crate documentation for why that is the whole design and not an omission.
    ///
    /// # Cancellation
    ///
    /// Safe at every point, and trivially so: the call only reads. Dropping it mid-check
    /// leaves a `dpkg-query` that may still be running and will exit on its own, and
    /// nothing else — no unpacked archive, no half-written config, no partial state to
    /// unwind. A fresh call afterwards produces the same progress sequence as a first
    /// one.
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

        match self.check_dependencies(scanner, &progress).await {
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
            BackendError::Unsupported { backend, operation } => BackendError::Other(format!(
                "{operation} is not supported by backend {backend} for scanner {scanner}"
            )),
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
    if set.len() == 1 { set.first() } else { None }
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
    let dict = scanner
        .capabilities
        .extra
        .get("brother")
        .and_then(as_dict)?;
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
        .or_else(|| {
            query_value(uri, "hostname")
                .map(|value| value.trim_end_matches('.').to_ascii_lowercase())
        })
        .or_else(|| {
            query_value(uri, "zc").map(|value| value.trim_end_matches('.').to_ascii_lowercase())
        })
        .or_else(|| usb_endpoint(uri))
        .or_else(|| ipv4(uri))
}

fn stable_hint_from_uri(uri: &str) -> Option<String> {
    query_value(uri, "serial")
        .map(str::to_owned)
        .or_else(|| {
            query_value(uri, "hostname")
                .map(|value| value.trim_end_matches('.').to_ascii_lowercase())
        })
        .or_else(|| {
            query_value(uri, "zc").map(|value| value.trim_end_matches('.').to_ascii_lowercase())
        })
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
    use std::fs;
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::ExitStatus;
    use std::sync::Mutex;
    use std::time::Duration;

    use scanbus_core::PairingState;
    use tempfile::TempDir;

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
                brscan4: Presence::Present,
                brscan5: Presence::Absent,
                brscan_skey: Presence::Present,
            },
        )
        .unwrap();

        assert_eq!(scanners.len(), 1);
        assert_eq!(scanners[0].name, "MFC-L2710DW");
        assert_eq!(scanners[0].address, "brw001122334455.local:80");
        assert_eq!(
            scanners[0].id.as_str(),
            "brother_brw001122334455_2Elocal_3A80"
        );
        let brother = scanners[0]
            .capabilities
            .extra
            .get("brother")
            .and_then(as_dict)
            .unwrap();
        assert_eq!(
            brother.get("driver"),
            Some(&Value::Str("brscan4".to_owned()))
        );
        assert_eq!(brother.get("driver_installed"), Some(&Value::Bool(true)));
        assert_eq!(scanners[0].capabilities.buttons.count, 4);
    }

    #[test]
    fn unknown_models_keep_zero_buttons_and_no_driver_guess() {
        let scanners =
            scanners_from_sightings(
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

    // ---------------------------------------------------------------- install checks

    /// A `dpkg-query` that answers from a table instead of from this machine's status
    /// database, and records every argv it was handed.
    ///
    /// The recording is the point: it is what lets a test state "these are the only
    /// programs the backend can run, and both are read-only" as an assertion rather
    /// than as a comment.
    #[derive(Debug, Default)]
    struct RecordingRunner {
        invocations: Mutex<Vec<Vec<String>>>,
        /// Packages `dpkg-query -W` reports as `installed`.
        registered: BTreeSet<&'static str>,
        /// What `dpkg-query -L` lists for a package.
        files: BTreeMap<&'static str, Vec<String>>,
        /// Slept before answering, so a test can drop the call while it is in flight.
        delay: Duration,
    }

    impl RecordingRunner {
        fn programs(&self) -> Vec<String> {
            self.invocations
                .lock()
                .unwrap()
                .iter()
                .map(|argv| argv[0].clone())
                .collect()
        }

        fn argv(&self) -> Vec<Vec<String>> {
            self.invocations.lock().unwrap().clone()
        }
    }

    impl CommandRunner for RecordingRunner {
        fn run(&self, program: &Path, args: &[&str]) -> io::Result<Output> {
            let mut argv = vec![program.display().to_string()];
            argv.extend(args.iter().map(|arg| (*arg).to_owned()));
            self.invocations.lock().unwrap().push(argv);

            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }

            let package = args.last().copied().unwrap_or_default();
            let (code, stdout) = match args.first().copied() {
                Some("-W") if self.registered.contains(package) => (0, "installed\n".to_owned()),
                Some("-L") => match self.files.get(package) {
                    Some(files) => (0, format!("{}\n", files.join("\n"))),
                    None => (1, String::new()),
                },
                _ => (1, String::new()),
            };

            Ok(Output {
                status: ExitStatus::from_raw(code << 8),
                stdout: stdout.into_bytes(),
                stderr: Vec::new(),
            })
        }
    }

    /// A sysroot with `dpkg-query` present but nothing else, plus the files asked for.
    fn sysroot(files: &[&str]) -> TempDir {
        let root = tempfile::tempdir().unwrap();
        for file in files {
            let path = root.path().join(file.trim_start_matches('/'));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"stub").unwrap();
        }
        root
    }

    fn backend(root: &TempDir, runner: Arc<RecordingRunner>) -> BrotherBackend {
        // `dpkg_query_path` has to exist as a file for the packaging query to be tried
        // at all; the runner is what decides what it answers.
        let dpkg_query = root.path().join("dpkg-query");
        fs::write(&dpkg_query, b"stub").unwrap();

        BrotherBackend {
            dpkg_query_path: dpkg_query,
            sysroot: root.path().to_path_buf(),
            runner,
            ..BrotherBackend::default()
        }
    }

    /// The MFC-L2710DW of this machine, as discovery hands it to `ensure_installed`.
    fn brother_mfc() -> ScannerInfo {
        scanners_from_sightings(
            vec![
                parse_scanimage_line("device 'brother4:net1;dev0' is 'Brother MFC-L2710DW'")
                    .unwrap(),
            ],
            InstalledState::default(),
        )
        .unwrap()
        .remove(0)
    }

    async fn ensure_installed_progress(
        backend: &BrotherBackend,
        scanner: &ScannerInfo,
    ) -> (Result<(), BackendError>, Vec<PairingProgress>) {
        let (sender, mut receiver) = mpsc::channel(8);
        let outcome = backend.ensure_installed(scanner, sender).await;

        let mut progress = Vec::new();
        while let Ok(step) = receiver.try_recv() {
            progress.push(step);
        }
        (outcome, progress)
    }

    /// The boring case this machine is in: both packages present, so pairing walks
    /// straight through — but still through `installing_backend`, because that is the
    /// state a client's progress UI is written against.
    #[tokio::test]
    async fn present_dependencies_still_pass_through_installing_backend() {
        let root = sysroot(&["/opt/brother/scanner/brscan4", "/usr/bin/brscan-skey"]);
        let runner = Arc::new(RecordingRunner::default());
        let backend = backend(&root, Arc::clone(&runner));

        let (outcome, progress) = ensure_installed_progress(&backend, &brother_mfc()).await;

        outcome.unwrap();
        assert_eq!(
            progress,
            vec![
                PairingProgress::Checking {
                    message: "checking Brother dependencies for MFC-L2710DW".to_owned(),
                },
                PairingProgress::Installing {
                    package: "brscan4".to_owned(),
                    percent: None,
                },
                PairingProgress::Installing {
                    package: "brscan-skey".to_owned(),
                    percent: None,
                },
                PairingProgress::Ready,
            ]
        );
        assert!(
            progress
                .iter()
                .any(|step| step.pairing_state() == Some(PairingState::InstallingBackend))
        );
        // Files on disk answer the question on their own: no subprocess at all.
        assert!(runner.programs().is_empty());
    }

    /// `brscan4` masked from detection: named, sourced, and refused — nothing installed.
    #[tokio::test]
    async fn a_missing_driver_is_refused_by_name_and_by_where_it_comes_from() {
        let root = sysroot(&["/usr/bin/brscan-skey"]);
        let runner = Arc::new(RecordingRunner::default());
        let backend = backend(&root, Arc::clone(&runner));

        let (outcome, progress) = ensure_installed_progress(&backend, &brother_mfc()).await;

        let error = outcome.unwrap_err();
        let BackendError::InstallFailed { package, detail } = &error else {
            panic!("expected an install failure, got {error:?}");
        };
        assert_eq!(package, "brscan4");
        assert!(detail.contains("brscan4"), "{detail}");
        assert!(detail.contains(BROTHER_SUPPORT_SITE), "{detail}");
        assert!(detail.contains("scanbus does not install it"), "{detail}");

        // The client sees the same story on the property as the caller does on the Err.
        assert_eq!(
            progress.last(),
            Some(&PairingProgress::Failed {
                message: error.to_string(),
            })
        );
        assert_eq!(
            progress.last().unwrap().pairing_state(),
            Some(PairingState::Failed(error.to_string()))
        );
        // It gave up at the driver: `brscan-skey` was never announced as a step.
        assert_eq!(
            progress
                .iter()
                .filter(|step| matches!(step, PairingProgress::Installing { .. }))
                .count(),
            1
        );
    }

    /// A package can be installed and its files removed, and the status database keeps
    /// saying `installed`. Believing it defers the failure to `start_listening`, where
    /// it looks like a SANE problem.
    #[tokio::test]
    async fn a_registered_package_with_no_files_is_not_installed() {
        let root = sysroot(&["/usr/bin/brscan-skey"]);
        let runner = Arc::new(RecordingRunner {
            registered: BTreeSet::from(["brscan4"]),
            files: BTreeMap::from([(
                "brscan4",
                vec!["/opt/brother/scanner/brscan4/Brsane4.ini".to_owned()],
            )]),
            ..RecordingRunner::default()
        });
        let backend = backend(&root, Arc::clone(&runner));

        let (outcome, _) = ensure_installed_progress(&backend, &brother_mfc()).await;

        let BackendError::InstallFailed { package, detail } = outcome.unwrap_err() else {
            panic!("expected an install failure");
        };
        assert_eq!(package, "brscan4");
        assert!(detail.contains("none of its files are on disk"), "{detail}");
        assert!(detail.contains("reinstall"), "{detail}");
    }

    /// The SANE library path is architecture-dependent, so the hard-coded list is a
    /// fast path, not the answer. The package's own file list is what settles it.
    #[tokio::test]
    async fn a_driver_installed_outside_the_known_paths_is_found_through_the_package() {
        let root = sysroot(&[
            "/usr/lib/aarch64-linux-gnu/sane/libsane-brother4.so",
            "/usr/bin/brscan-skey",
        ]);
        let runner = Arc::new(RecordingRunner {
            registered: BTreeSet::from(["brscan4"]),
            files: BTreeMap::from([(
                "brscan4",
                vec![
                    "/usr/share/doc/brscan4".to_owned(),
                    "/usr/lib/aarch64-linux-gnu/sane/libsane-brother4.so".to_owned(),
                ],
            )]),
            ..RecordingRunner::default()
        });
        let backend = backend(&root, Arc::clone(&runner));

        let (outcome, _) = ensure_installed_progress(&backend, &brother_mfc()).await;

        outcome.unwrap();
        assert_eq!(
            runner.argv().first().map(|argv| argv[1].clone()),
            Some("-W".to_owned())
        );
    }

    /// `CancelPairing()` drops the future, possibly while a probe is running. The check
    /// only reads, so there is nothing to unwind — and the proof is that a fresh call
    /// reports exactly the sequence a first one would.
    #[tokio::test]
    async fn cancelling_the_check_leaves_nothing_behind() {
        let root = sysroot(&["/usr/lib/aarch64-linux-gnu/sane/libsane-brother4.so"]);
        let runner = Arc::new(RecordingRunner {
            registered: BTreeSet::from(["brscan4", "brscan-skey"]),
            files: BTreeMap::from([
                (
                    "brscan4",
                    vec!["/usr/lib/aarch64-linux-gnu/sane/libsane-brother4.so".to_owned()],
                ),
                ("brscan-skey", vec!["/usr/bin/brscan-skey".to_owned()]),
            ]),
            delay: Duration::from_millis(200),
            ..RecordingRunner::default()
        });
        let backend = backend(&root, Arc::clone(&runner));
        let scanner = brother_mfc();

        let (sender, _receiver) = mpsc::channel(8);
        let cancelled = tokio::time::timeout(
            Duration::from_millis(20),
            backend.ensure_installed(&scanner, sender),
        )
        .await;
        assert!(
            cancelled.is_err(),
            "the check should still have been running"
        );

        // `brscan-skey`'s files were never there; the cancelled run cannot have created
        // them, and the second run has to reach the same verdict from the same evidence.
        assert!(!root.path().join("usr/bin/brscan-skey").exists());
        fs::create_dir_all(root.path().join("usr/bin")).unwrap();
        fs::write(root.path().join("usr/bin/brscan-skey"), b"stub").unwrap();

        let (outcome, progress) = ensure_installed_progress(&backend, &scanner).await;

        outcome.unwrap();
        assert_eq!(
            progress,
            vec![
                PairingProgress::Checking {
                    message: "checking Brother dependencies for MFC-L2710DW".to_owned(),
                },
                PairingProgress::Installing {
                    package: "brscan4".to_owned(),
                    percent: None,
                },
                PairingProgress::Installing {
                    package: "brscan-skey".to_owned(),
                    percent: None,
                },
                PairingProgress::Ready,
            ]
        );
    }

    /// The allowlist, as an assertion: two programs, both read-only queries.
    ///
    /// This is what [`ensure_installed`](ScannerBackend::ensure_installed) not being
    /// able to install anything means concretely. `dpkg-query` is not `dpkg`: `-W` and
    /// `-L` read the status database, and neither has a mode that unpacks a `.deb`.
    #[tokio::test]
    async fn the_install_path_only_ever_runs_read_only_queries() {
        let root = sysroot(&[]);
        let runner = Arc::new(RecordingRunner {
            registered: BTreeSet::from(["brscan4"]),
            ..RecordingRunner::default()
        });
        let backend = backend(&root, Arc::clone(&runner));

        let (outcome, _) = ensure_installed_progress(&backend, &brother_mfc()).await;
        assert!(outcome.is_err());

        let dpkg_query = backend.dpkg_query_path.display().to_string();
        for argv in runner.argv() {
            assert_eq!(argv[0], dpkg_query, "unexpected program: {argv:?}");
            assert!(
                matches!(argv[1].as_str(), "-W" | "-L"),
                "unexpected dpkg-query mode: {argv:?}"
            );
        }
        assert!(!runner.argv().is_empty(), "the probe never ran");
    }

    /// The same claim, checked against the source rather than against one run: the
    /// mechanisms an install would need are absent from the crate.
    ///
    /// **The invariant is "this backend cannot install anything", not "this backend does
    /// not use the network."** It used to read as the latter, because at the time the
    /// only thing the crate did was run `scanimage` and stat some files. That is no
    /// longer true and must not be tightened back: [`crate::skey`] exists precisely so
    /// that scanbus can speak SNMP to a printer on the local network *instead of*
    /// requiring two `.deb` files from a vendor download form. A UDP socket to
    /// `192.168.x.y:161` is not a download.
    ///
    /// So what is forbidden is the machinery of *acquiring and installing software*: a
    /// package manager, an HTTP client, a downloader, privilege escalation. `TcpStream`
    /// stays on the list not because TCP is forbidden in principle but because nothing
    /// in this protocol uses it — the day a Phoenix-firmware model needs
    /// `POST /phoenix/mib`, that is a decision to argue for in a review, and this
    /// assertion is what forces the argument to happen.
    ///
    /// Comment lines are excluded on purpose — the crate documentation has to be able
    /// to name what it refuses to do — and so is each file's test module, which names
    /// them in order to forbid them.
    #[test]
    fn the_backend_has_no_way_to_install_anything() {
        // Every non-test source file in the crate, not just this one: `skey` is where
        // the code that talks to a device now lives, and a guard that only covered
        // lib.rs would have stopped meaning anything the moment it was added.
        let sources = [
            ("lib.rs", include_str!("lib.rs")),
            ("skey/mod.rs", include_str!("skey/mod.rs")),
            ("skey/snmp.rs", include_str!("skey/snmp.rs")),
            ("skey/register.rs", include_str!("skey/register.rs")),
            ("skey/event.rs", include_str!("skey/event.rs")),
            ("skey/fields.rs", include_str!("skey/fields.rs")),
        ];

        let production = |source: &str| {
            source
                .split("#[cfg(test)]")
                .next()
                .expect("the test module marker splits the file")
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n")
        };

        for (name, source) in sources {
            let production = production(source);
            for forbidden in [
                "pkexec",
                "sudo",
                "apt-get",
                "apt install",
                "dpkg -i",
                "--install",
                "--unpack",
                "reqwest",
                "ureq",
                "hyper",
                "TcpStream",
                "curl",
                "wget",
            ] {
                assert!(
                    !production.contains(forbidden),
                    "{name} must not reach for {forbidden}"
                );
            }
        }

        // One process spawn in the crate, inside the runner every probe goes through.
        let spawns: usize = sources
            .iter()
            .map(|(_, source)| production(source).matches("Command::new").count())
            .sum();
        assert_eq!(spawns, 1, "every subprocess must go through CommandRunner");
    }

    /// The protocol modules are pure: no socket, no filesystem, no clock.
    ///
    /// This is what makes `cargo test -p scanbus-backend-brother` runnable with no
    /// hardware and no network — the property the whole "parse it before you socket it"
    /// order exists to buy, so it is asserted rather than left as an intention.
    #[test]
    fn the_skey_protocol_modules_open_nothing() {
        let sources = [
            ("skey/mod.rs", include_str!("skey/mod.rs")),
            ("skey/snmp.rs", include_str!("skey/snmp.rs")),
            ("skey/register.rs", include_str!("skey/register.rs")),
            ("skey/event.rs", include_str!("skey/event.rs")),
            ("skey/fields.rs", include_str!("skey/fields.rs")),
        ];
        for (name, source) in sources {
            let production = source
                .split("#[cfg(test)]")
                .next()
                .expect("the test module marker splits the file")
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");

            for forbidden in [
                "UdpSocket",
                "std::fs",
                "File::",
                "Command::new",
                "Instant::now",
                "SystemTime::now",
            ] {
                assert!(
                    !production.contains(forbidden),
                    "{name} is a protocol module and must stay free of {forbidden}"
                );
            }
        }
    }
}
