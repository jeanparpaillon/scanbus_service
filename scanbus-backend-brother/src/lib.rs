//! Brother backend: eSCL discovery and acquisition, plus the scan-key protocol.
//!
//! Discovery starts from `scanimage -L`, which already enumerates every eSCL-capable
//! device through `sane-airscan` — one probe to parse instead of a fresh mDNS
//! implementation before the basics are tested.
//!
//! # Nothing from Brother's website is needed, or used
//!
//! `brscan4`/`brscan5` and `brscan-skey` used to be this backend's dependencies, and both
//! are `.deb` files outside any apt repository. They are gone
//! ([`brother-skeyless-backend.md`] §2, and 5.10): the panel protocol is spoken here
//! ([`skey`]) and the image comes over **eSCL**, which every Brother network model of the
//! last several years offers and which `sane-airscan` — one `apt install`, from the
//! distribution archive — already speaks. On the development machine's MFC-J5335DW that
//! is the same device the vendor driver reaches:
//!
//! ```text
//! device `brother4:net1;dev0' is a Brother MFC-J5335DW MFC-J5335DW
//! device `airscan:e0:Brother MFC-J5335DW' is a eSCL Brother MFC-J5335DW ip=192.168.1.3
//! ```
//!
//! A `brother4:`/`brother5:` sighting is therefore still *read* — it is evidence that a
//! Brother device exists, which keeps a device with no eSCL discoverable so that pairing
//! can explain itself — but it is never an acquisition path and never selects a driver.
//! There is no vendor package to check for, no per-model driver table, and
//! [`Driver`](https://github.com/jeanparpaillon/scanbus_service/issues/69) is gone with
//! them.
//!
//! # This backend still never installs anything
//!
//! [`ensure_installed`](BrotherBackend::ensure_installed) *verifies, or refuses*: it
//! reports what is missing, by name and by where it comes from, and stops. What changed
//! with 5.10 is only what it names — `sane-airscan`, which the user installs with `apt`,
//! rather than two packages behind a vendor download form. Assisted installation is still
//! a later, separate step, and needs pieces this backend does not have.
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
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use scanbus_backend_common::{DEFAULT_SCANIMAGE_HELPER, ScanimageConfig, fetch_pages_via_scanimage};
use scanbus_core::{
    BackendError, ButtonsCapability, Capabilities, ColorMode, PairingProgress, ProfileKind,
    RawPage, RestoreDisposition, ScanTrigger, ScannerBackend, ScannerId, ScannerInfo, Source,
    Status, Value,
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

pub mod acquisition;
pub mod listener;
pub mod registrar;
pub mod skey;

use acquisition::{ButtonMapping, EsclDevice};
use listener::{BindError, Inert, Listener};
use registrar::{SnmpTransport, UdpSnmp, probe_scan_to_pc};
use skey::register::{Function, LISTENER_PORT};
use skey::snmp::DEFAULT_COMMUNITY;

/// Backend identifier, as it will be reported by [`ScannerBackend::id`].
pub const ID: &str = "brother-skey";

const SCANNER_ID_BACKEND: &str = "brother";
const SCANNER_BACKEND_NAME: &str = "escl:brother";

/// How the one dependency is installed, named in every "not installed" message.
///
/// The whole point of 5.10: the old messages had to send the user to a vendor download
/// form, because `apt install brscan4` works on no distribution. This one is a command
/// that works.
const AIRSCAN_INSTALL_HINT: &str = "apt install sane-airscan";

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

/// The one package acquisition needs, and the only dependency pairing checks.
///
/// The `.so` path is architecture-dependent, which is exactly why the file list is a fast
/// path rather than the answer — [`BrotherBackend::package_presence`] falls through to the
/// package's own file list for anything not spelled here.
const AIRSCAN_PROBE: PackageProbe = PackageProbe {
    package: "sane-airscan",
    files: &[
        "/usr/lib/x86_64-linux-gnu/sane/libsane-airscan.so.1",
        "/usr/lib/aarch64-linux-gnu/sane/libsane-airscan.so.1",
        "/usr/lib64/sane/libsane-airscan.so.1",
        "/usr/lib/sane/libsane-airscan.so.1",
    ],
};

/// `brscan-skey`, which is **not** a dependency any more and is still worth detecting.
///
/// Nothing requires it, nothing checks for it during pairing, and acquisition never goes
/// near it. It survives here for one message only: it is what normally holds UDP/54925,
/// so [`BrotherBackend::port_taken`] can say "this machine has it installed" instead of
/// sending the user hunting for an unnamed process. See [`listener`] for why the port is
/// not shared.
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
    /// The packaged wrapper acquisition runs, so that the daemon's own `scanimage` path
    /// is one file the distribution owns rather than a guess per backend.
    scanimage_helper_path: PathBuf,
    dpkg_query_path: PathBuf,
    /// Prefix every probed path is resolved against. `/` outside tests; a tempdir in
    /// them, which is what lets a test mask `brscan4` without touching the machine.
    sysroot: PathBuf,
    runner: Arc<dyn CommandRunner>,
    /// How the scan-key OID is asked about. A seam for the same reason
    /// [`CommandRunner`] is one: it is what lets a test stand in a device that refuses.
    transport: Arc<dyn SnmpTransport>,
    /// What each device answered when it was asked whether it does scan-to-PC at all.
    ///
    /// Shared with every clone of the backend, because [`discover`](ScannerBackend::discover)
    /// runs on a clone and is where the answer has to surface. Written at pairing time
    /// and read at discovery time — see [`BrotherBackend::note_scan_to_pc`] for why the
    /// question is not asked on every discovery instead.
    scan_to_pc: Arc<Mutex<BTreeMap<ScannerId, ScanToPc>>>,
    /// The one UDP/54925 socket every paired Brother sends its panel presses to.
    ///
    /// Shared with every clone of the backend, because the port is a process-wide resource
    /// and not a per-scanner one ([`listener`]). Binds nothing until the first
    /// [`start_listening`](ScannerBackend::start_listening).
    listener: Arc<Listener>,
    /// The eSCL device each scanner is acquired from, as discovery last saw it.
    ///
    /// [`fetch_pages`](ScannerBackend::fetch_pages) is handed a [`ScannerId`] and a
    /// trigger id and nothing else, so the device name and the capability lists it needs
    /// have to be carried here from discovery — the same shape HP's `sane_names` map has,
    /// for the same reason. Shared with every clone: `discover` runs on one and
    /// `fetch_pages` on another.
    escl: Arc<Mutex<BTreeMap<ScannerId, EsclDevice>>>,
    /// What the daemon has assigned to each panel key, per scanner.
    ///
    /// Written by [`set_button_mapping`](ScannerBackend::set_button_mapping) and read by
    /// [`fetch_pages`](ScannerBackend::fetch_pages), which is the only reason the backend
    /// keeps it: a press carries an index, and the profile that decides how to scan lives
    /// on the daemon's side of the seam (1.3). Not durable — the daemon's own button
    /// store is (4.1), and it replays the assignments on startup (4.2).
    buttons: Arc<Mutex<BTreeMap<(ScannerId, u32), ButtonMapping>>>,
}

impl Default for BrotherBackend {
    fn default() -> Self {
        Self {
            scanimage_path: PathBuf::from("/usr/bin/scanimage"),
            scanimage_helper_path: PathBuf::from(DEFAULT_SCANIMAGE_HELPER),
            dpkg_query_path: PathBuf::from("/usr/bin/dpkg-query"),
            sysroot: PathBuf::from("/"),
            runner: Arc::new(SystemCommandRunner),
            transport: Arc::new(UdpSnmp::default()),
            scan_to_pc: Arc::new(Mutex::new(BTreeMap::new())),
            listener: Arc::new(Listener::new(LISTENER_PORT)),
            escl: Arc::new(Mutex::new(BTreeMap::new())),
            buttons: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }
}

/// What a device has said about scan-to-PC, when it was asked.
///
/// Only ever holds an answer the *device* gave. A printer that was switched off when
/// pairing ran is absent from the map rather than present as unavailable: silence is a
/// statement about that minute, and turning it into a permanent "this model has no
/// buttons" would need a power cycle and a re-pair to undo.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ScanToPc {
    /// The device knows the scan-key OID. Its panel keys are worth offering.
    Available,
    /// It cannot be registered, and the reason is worth carrying to the user.
    Unavailable { reason: String },
}

/// How one `scanimage -L` line reaches the device.
///
/// The vendor SANE backends collapse into a single [`Transport::Vendor`]: telling
/// `brother4:` from `brother5:` only ever existed to pick which `.deb` to demand, and
/// neither is an acquisition path any more. The variant stays because the *sighting*
/// still means something — a Brother device is there — which is what keeps a model with
/// no eSCL discoverable instead of silently absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    Airscan,
    Escl,
    Vendor,
    Other,
}

impl Transport {
    /// Whether this sighting is one acquisition can run against.
    const fn is_escl(self) -> bool {
        matches!(self, Self::Airscan | Self::Escl)
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Airscan => "airscan",
            Self::Escl => "escl",
            Self::Vendor => "vendor",
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

/// What this machine can acquire with, which is now one question and not three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct InstalledState {
    airscan: Presence,
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
        let scan_to_pc = self.scan_to_pc.lock().expect("scan-to-PC notes").clone();
        let scanners = scanners_from_sightings(
            parse_scanimage_output(&String::from_utf8_lossy(&output.stdout)),
            installed,
            &scan_to_pc,
        )?;

        // Discovery is the only place the eSCL device name is seen, and `fetch_pages`
        // gets a `ScannerId` and nothing else. Recorded rather than re-derived, and
        // *replaced* on every run: `sane-airscan` numbers its devices by discovery order,
        // so `airscan:e0:` and `airscan:e2:` were the same printer twice on this machine
        // within one session — a name cached once and never refreshed opens the wrong
        // device, or none.
        {
            let mut escl = self.escl.lock().expect("eSCL device notes");
            for scanner in &scanners {
                if let Some(device) = escl_device(scanner) {
                    escl.insert(scanner.id.clone(), device);
                } else {
                    escl.remove(&scanner.id);
                }
            }
        }

        Ok(scanners)
    }

    /// Ask the device whether it does scan-to-PC at all, and remember what it said.
    ///
    /// **This never fails pairing.** A device that refuses the registration OID is an
    /// older or newer generation ([`brother-skeyless-backend.md`] §4 — the arch notes
    /// record models documenting TCP 5566 and 54921 instead), and it stays a perfectly
    /// good pull scanner. The consequence of a refusal is `buttons.count = 0` and a
    /// reason in the capability dict, not an error out of `Pair()`.
    ///
    /// A **read**, not a registration: pairing must not put an entry on the panel that
    /// nothing is listening behind — the listener is 5.9 and the mapping of keys to
    /// profiles is 5.11. `GetRequest` on the same OID answers the same question and
    /// changes nothing on the device.
    ///
    /// Asked here rather than during discovery because discovery must stay cheap and
    /// must not block: one unreachable printer would otherwise add
    /// [`registrar::RESPONSE_TIMEOUT`] to every `discover()` on the machine. Pairing
    /// already talks about one device, at a moment the user is waiting for it.
    ///
    /// [`brother-skeyless-backend.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/brother-skeyless-backend.md
    async fn note_scan_to_pc(&self, scanner: &ScannerInfo) {
        let note = match device_ipv4(scanner) {
            Some(device) => {
                match probe_scan_to_pc(&*self.transport, device, DEFAULT_COMMUNITY).await {
                    Ok(()) => ScanToPc::Available,
                    Err(error) if error.is_refusal() => ScanToPc::Unavailable {
                        reason: error.to_string(),
                    },
                    // Silence says nothing about the model. Leave the scanner as
                    // discovery described it and ask again at the next pairing.
                    Err(error) => {
                        debug!(
                            scanner = %scanner.id,
                            %error,
                            "could not ask this device about scan-to-PC; leaving its buttons as \
                             discovery reported them",
                        );
                        return;
                    }
                }
            }
            // Nothing to send SNMP to. A USB scanner is the honest case — the vendor
            // daemon has a separate USB path for panel keys and scanbus does not
            // implement it — and it is recorded as such rather than left implying that
            // the four keys work.
            None => match usb_endpoint(&scanner.address) {
                Some(endpoint) => ScanToPc::Unavailable {
                    reason: format!(
                        "scanbus registers scan-to-PC over the network, and {endpoint} is a USB \
                         connection; connect this scanner to the network to use its panel keys"
                    ),
                },
                None => {
                    debug!(
                        scanner = %scanner.id,
                        address = %scanner.address,
                        "no IPv4 address to ask about scan-to-PC",
                    );
                    return;
                }
            },
        };

        if let ScanToPc::Unavailable { reason } = &note {
            info!(scanner = %scanner.id, %reason, "this scanner reports no panel keys");
        }
        self.scan_to_pc
            .lock()
            .expect("scan-to-PC notes")
            .insert(scanner.id.clone(), note);
    }

    /// The address this scanner's presses will arrive from, or why none ever will.
    ///
    /// Both answers come from what the *device* said or from what discovery found, never
    /// from a guess about the model: a printer that was switched off during pairing is
    /// absent from the scan-to-PC notes rather than recorded as key-less
    /// ([`note_scan_to_pc`](Self::note_scan_to_pc)), so it gets a real listener and a
    /// chance to work.
    fn listen_target(&self, scanner: &ScannerInfo) -> Result<Ipv4Addr, String> {
        let Some(device) = device_ipv4(scanner) else {
            return Err(match usb_endpoint(&scanner.address) {
                Some(endpoint) => format!(
                    "{endpoint} is a USB connection, and scan-to-PC registration is a network \
                     protocol"
                ),
                None => format!(
                    "discovery found no IPv4 address for {}, so nothing can be registered to \
                     send here",
                    scanner.address
                ),
            });
        };
        match self
            .scan_to_pc
            .lock()
            .expect("scan-to-PC notes")
            .get(&scanner.id)
        {
            Some(ScanToPc::Unavailable { reason }) => Err(reason.clone()),
            Some(ScanToPc::Available) | None => Ok(device),
        }
    }

    /// What to tell the user when UDP/54925 already has an owner.
    ///
    /// The package check is what makes the message worth reading: naming `brscan-skey` on a
    /// machine that has never had it installed would send someone hunting a package that is
    /// not there, and naming "another process" on a machine where it *is* installed would
    /// hide the answer. Only reached on a failed bind, so its `dpkg-query` costs nothing in
    /// the normal path.
    fn port_taken(&self, port: u16) -> String {
        let shared = format!(
            "scanbus will not share UDP/{port}: the kernel would hand each panel press to one \
             of the two processes at random, so every other press would go missing. This \
             scanner stays discovered and can still be scanned from the host; only its panel \
             keys are unavailable."
        );
        match self.package_presence(BRSCAN_SKEY_PROBE).unwrap_or_default() {
            Presence::Present => format!(
                "UDP/{port} is already bound, and brscan-skey is installed on this machine, \
                 which is what holds it. Stop it with `brscan-skey -t`, keep whatever launches \
                 it from starting it again, and connect this scanner again. {shared}"
            ),
            Presence::RegisteredButMissing | Presence::Absent => format!(
                "UDP/{port} is already bound by another process. brscan-skey is what normally \
                 holds that port and does not appear to be installed here, so run \
                 `ss -lunp | grep {port}` to see what does. {shared}"
            ),
        }
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

    /// `sane-airscan`, and then that this device actually offers eSCL.
    ///
    /// Two steps, and they fail for different reasons: a machine with no `sane-airscan`
    /// is one `apt install` from working, and a device that only ever appeared as
    /// `brother4:` will not be fixed by installing anything. Both are still
    /// [`PairingProgress::Installing`] steps, because the observable sequence is what a
    /// client's progress UI is written against — see [`Self::check_dependency`].
    ///
    /// The vendor packages are checked for *nothing*. `brscan4`, `brscan5` and
    /// `brscan-skey` may be installed or not; acquisition goes over eSCL either way, and
    /// the only thing this backend still asks about `brscan-skey` is whether it is what
    /// holds UDP/54925 when a bind fails ([`Self::port_taken`]).
    async fn check_dependencies(
        &self,
        scanner: &ScannerInfo,
        progress: &mpsc::Sender<PairingProgress>,
    ) -> Result<(), BackendError> {
        self.check_dependency(AIRSCAN_PROBE, progress).await?;
        Self::check_escl_device(scanner)
    }

    /// That discovery found an eSCL way in to *this* device.
    ///
    /// The degraded path of [`brother-skeyless-backend.md`] §4: a model with no eSCL is
    /// still discovered, so that pairing can say what is wrong instead of the scanner
    /// silently not being there. It fails pairing rather than deferring to the first
    /// press, because a panel entry that produces an error when it is used is worse than
    /// a pairing that explained itself while the user was watching.
    ///
    /// [`brother-skeyless-backend.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/brother-skeyless-backend.md
    fn check_escl_device(scanner: &ScannerInfo) -> Result<(), BackendError> {
        if escl_device(scanner).is_some() {
            return Ok(());
        }

        Err(BackendError::InstallFailed {
            package: AIRSCAN_PROBE.package.to_owned(),
            detail: format!(
                "{} was found, but not over eSCL: scanbus acquires Brother scans through \
                 sane-airscan and this device offered no eSCL interface to discovery. Check \
                 that `{AIRSCAN_INSTALL_HINT}` has been run, that the scanner has network \
                 scanning enabled, and rediscover — scanbus no longer uses brscan4/brscan5",
                scanner.name
            ),
        })
    }

    fn installed_state(&self) -> Result<InstalledState, BackendError> {
        Ok(InstalledState {
            airscan: self.package_presence(AIRSCAN_PROBE)?,
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
/// Names the package *and the command*, because the two failures need different actions.
/// This is the message 5.10 exists to change: it used to have to point at a vendor
/// download form, and now it points at the distribution archive.
fn missing_package_detail(probe: PackageProbe, presence: Presence) -> String {
    let package = probe.package;
    match presence {
        Presence::Present => format!("{package} is installed"),
        Presence::RegisteredButMissing => format!(
            "{package} is registered as installed but none of its files are on disk; \
             reinstall it with `{AIRSCAN_INSTALL_HINT} --reinstall` and pair again — \
             scanbus does not install it"
        ),
        Presence::Absent => format!(
            "{package} is not installed; scanbus acquires Brother scans over eSCL and \
             needs it. Run `{AIRSCAN_INSTALL_HINT}` and pair again — scanbus does not \
             install it"
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
    /// unwind. The scan-to-PC probe is a read too, of one OID, and a drop during it
    /// leaves the device exactly as it was and this backend with one fewer note than it
    /// would have had. A fresh call afterwards produces the same progress sequence as a
    /// first one.
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
                // After the dependencies and before `Ready`, because what it learns
                // changes what the scanner is reported to be able to do — and it emits
                // no progress step of its own, so the observable sequence a client is
                // written against is unchanged whether the device answers or not.
                self.note_scan_to_pc(scanner).await;
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

    /// Subscribes this scanner to the one UDP/54925 socket every Brother sends to.
    ///
    /// # A scanner that can never be pressed still gets a stream
    ///
    /// The pairing machine's last step is this call (1.4), so a backend that errors here
    /// can never reach `Paired=true`. Two of this backend's degraded paths are therefore
    /// **not** errors: a device with no IPv4 address — a USB scanner, whose panel keys the
    /// vendor daemon reaches over a USB path scanbus does not implement — and a device that
    /// answered the scan-key OID with a refusal ([`brother-skeyless-backend.md`] §4). Both
    /// are perfectly good pull scanners with `buttons.count = 0`, and both get an inert
    /// stream: open, so the daemon does not read it as a listener that died and retry it
    /// into `Status="error"`, and silent, because nothing will ever send them a datagram.
    ///
    /// `EADDRINUSE` is the opposite case and *is* an error, deliberately: a device that
    /// could do walk-up is being stopped by something on *this* machine, which is fixable
    /// and which the user has to be told about rather than left to discover from a panel
    /// entry that does nothing.
    ///
    /// [`brother-skeyless-backend.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/brother-skeyless-backend.md
    async fn start_listening(
        &self,
        scanner: &ScannerInfo,
    ) -> Result<BoxStream<'static, ScanTrigger>, BackendError> {
        let device = match self.listen_target(scanner) {
            Ok(device) => device,
            Err(reason) => {
                info!(
                    scanner = %scanner.id,
                    %reason,
                    "connected without a panel listener; this scanner has no keys to report",
                );
                return Ok(Box::pin(Inert::default()));
            }
        };

        match self.listener.subscribe(scanner.id.clone(), device).await {
            Ok(subscription) => Ok(Box::pin(subscription)),
            Err(BindError::Taken { port }) => Err(BackendError::Other(self.port_taken(port))),
            Err(error @ BindError::Io { .. }) => Err(BackendError::Other(format!(
                "scanner {}: {error}; its panel keys are unavailable until the socket can be \
                 opened, and it can still be scanned from the host",
                scanner.id
            ))),
        }
    }

    /// Drops this scanner's subscription, and the socket with it if it was the last.
    ///
    /// Awaits the receive task, so the file descriptor is gone before this returns — which
    /// is what makes a `Connect()`/`Disconnect()` cycle repeatable without a rebind ever
    /// racing our own closing socket. A scanner that is not subscribed — never was, or
    /// whose stream the caller already dropped — is a no-op and `Ok(())`, per the trait.
    async fn stop_listening(&self, scanner_id: &ScannerId) -> Result<(), BackendError> {
        self.listener.unsubscribe(scanner_id).await;
        Ok(())
    }

    async fn restore_disposition(&self, _scanner: &ScannerInfo) -> RestoreDisposition {
        RestoreDisposition::Paired
    }

    /// Records what the daemon assigned to a panel key, so acquisition knows how to scan.
    ///
    /// **This does not yet put the entry on the printer's panel.** Registering a function
    /// with the device — and stopping the refresh when a key is cleared, which is how an
    /// entry disappears from the LCD — is the registration half of the design
    /// ([`brother-skeyless-backend.md`] §3) and lands with 5.11. What is here is the half
    /// 5.10 needs: [`fetch_pages`](Self::fetch_pages) receives a trigger id, resolves it
    /// to the key that was pressed, and has to turn that key into a `--source` and a
    /// `--resolution`. The profile that decides those is the daemon's
    /// ([`ScanTrigger`] carries an index and never a profile, per 1.3), so the only way
    /// it can reach acquisition is for the assignment to be handed down and kept.
    ///
    /// `None` removes the assignment, per the trait: a cleared key is not the same as an
    /// unchanged one, and a scan started from a key nobody has assigned falls back to the
    /// device's own defaults rather than to whatever was mapped there last week.
    ///
    /// [`brother-skeyless-backend.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/brother-skeyless-backend.md
    async fn set_button_mapping(
        &self,
        scanner_id: &ScannerId,
        button_index: u32,
        profile: Option<ProfileKind>,
        options: &BTreeMap<String, Value>,
    ) -> Result<(), BackendError> {
        if Function::from_button_index(button_index).is_none() {
            return Err(BackendError::Other(format!(
                "scanner {scanner_id} has {} panel entries, indices 0..={}; asked to map \
                 button {button_index}",
                Function::ALL.len(),
                Function::ALL.len() - 1,
            )));
        }

        let mut buttons = self.buttons.lock().expect("brother button assignments");
        match profile {
            Some(profile) => {
                buttons.insert(
                    (scanner_id.clone(), button_index),
                    ButtonMapping {
                        profile,
                        options: options.clone(),
                    },
                );
            }
            None => {
                buttons.remove(&(scanner_id.clone(), button_index));
            }
        }
        Ok(())
    }

    /// Runs the scan the press asked for, over eSCL.
    ///
    /// Three lookups, each of which can fail for a reason the caller has to be able to
    /// tell apart:
    ///
    /// 1. **The press.** [`Listener::take_press`] both resolves the trigger id and
    ///    consumes it, which is what makes a second call for the same id
    ///    [`BackendError::UnknownJob`] rather than a second scan — the trait's
    ///    "callable exactly once per `trigger_id`".
    /// 2. **The device.** The eSCL name discovery last saw, from
    ///    [`BrotherBackend::escl`]. Absent means the scanner is paired but has not been
    ///    seen over eSCL since this daemon started; `NotReachable` says so, because
    ///    rediscovering is the fix and reinstalling nothing is.
    /// 3. **The assignment**, which may legitimately be absent — see
    ///    [`acquisition::scanimage_args`].
    ///
    /// The transfer itself is [`fetch_pages_via_scanimage`], unchanged from HP's (6.3):
    /// the ADF batching, the partial-PNM detection and the end-of-feed rule are one
    /// implementation in `scanbus-backend-common`, not two.
    async fn fetch_pages(
        &self,
        scanner_id: &ScannerId,
        trigger_id: &str,
    ) -> Result<BoxStream<'static, Result<RawPage, BackendError>>, BackendError> {
        let Some(press) = self.listener.take_press(scanner_id, trigger_id) else {
            return Err(BackendError::UnknownJob {
                scanner: scanner_id.clone(),
                job: trigger_id.to_owned(),
            });
        };

        let device = self
            .escl
            .lock()
            .expect("eSCL device notes")
            .get(scanner_id)
            .cloned();
        let Some(device) = device else {
            return Err(BackendError::NotReachable {
                scanner: scanner_id.clone(),
                detail: format!(
                    "no eSCL device is on record for {scanner_id}; run a discovery so the \
                     scanner is seen over eSCL again, then press the key once more"
                ),
            });
        };

        let button = press.function.button_index();
        let mapping = self
            .buttons
            .lock()
            .expect("brother button assignments")
            .get(&(scanner_id.clone(), button))
            .cloned();
        let acquisition = acquisition::scanimage_args(mapping.as_ref(), &device);

        info!(
            scanner = %scanner_id,
            trigger = %trigger_id,
            function = %press.function,
            device = %device.device_uri,
            args = ?acquisition.args,
            "acquiring over eSCL",
        );

        let mut config = ScanimageConfig::new(scanner_id.clone(), device.device_uri.clone());
        config.program = self.scanimage_helper_path.clone();
        config.resolution_dpi = acquisition.resolution_dpi;
        config.extra_args = acquisition.args;
        fetch_pages_via_scanimage(config).await
    }
}

fn scanners_from_sightings(
    sightings: Vec<Sighting>,
    installed: InstalledState,
    scan_to_pc: &BTreeMap<ScannerId, ScanToPc>,
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
            capabilities: capabilities_for_sighting(&sighting, installed, scan_to_pc.get(&id)),
            status: Status::Online,
        };
        // Inverted by 5.10: the eSCL sighting is the one acquisition runs against, so it
        // has to be the one that survives dedup. A `brother4:` line for the same device
        // only wins when it is the *only* sighting, which is the degraded case where the
        // scanner is reported so that pairing can explain why it cannot be used.
        let precedence = if sighting.transport.is_escl() { 0 } else { 1 };
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
    if uri.starts_with("brother4:") || uri.starts_with("brother5:") {
        Transport::Vendor
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
    matches!(sighting.transport, Transport::Vendor)
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
        if !sighting.transport.is_escl() || !is_brother_sighting(sighting) {
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

/// What one sighting can do, including what the device itself said about its panel keys.
///
/// `scan_to_pc` is `None` for a scanner that has never been asked — every scanner, until
/// it is paired. The four panel entries stand until the device contradicts them, which is
/// the right way round: a scanner nobody has paired yet is not evidence of anything, and
/// a `count` that dropped to zero on its own would be indistinguishable from a bug.
///
/// # No model table
///
/// 5.1 answered "what can this scanner do?" from a five-model lookup on the name, which
/// meant every other Brother — the development machine's MFC-J5335DW included — was
/// published with an empty resolution list and no sources at all. It is gone. What
/// replaces it is what the sighting itself says: the `escl:` backend prints its paper
/// paths into the description (`… adf,platen scanner`), and where a sighting is silent
/// the eSCL baseline below is used and marked as such, rather than a guess from a name.
fn capabilities_for_sighting(
    sighting: &Sighting,
    installed: InstalledState,
    scan_to_pc: Option<&ScanToPc>,
) -> Capabilities {
    let mut capabilities = escl_capabilities(sighting);
    let mut extra = BTreeMap::from([(
        "device_uri".to_owned(),
        Value::Str(sighting.device_uri.clone()),
    )]);
    extra.insert(
        "transport".to_owned(),
        Value::Str(sighting.transport.as_str().to_owned()),
    );
    // The acquisition choice, visible in `scanbus show --json`: which device name a scan
    // will be run against, or nothing at all when this Brother was only ever seen through
    // the vendor SANE backend, which scanbus no longer uses.
    if sighting.transport.is_escl() {
        extra.insert(
            "acquisition_uri".to_owned(),
            Value::Str(sighting.device_uri.clone()),
        );
    }
    extra.insert(
        "airscan_installed".to_owned(),
        Value::Bool(installed.airscan.is_usable()),
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

    match scan_to_pc {
        Some(ScanToPc::Available) => {
            extra.insert("scan_to_pc".to_owned(), Value::Bool(true));
        }
        Some(ScanToPc::Unavailable { reason }) => {
            // Zero buttons whatever the panel protocol offers in general: the four entries
            // are what the firmware defines, and this is the device's own answer about
            // whether it will accept any of them. Nothing else about the
            // scanner changes — it stays discovered, pairable, and scannable.
            capabilities.buttons = ButtonsCapability {
                count: 0,
                label_configurable: false,
                labels: Vec::new(),
            };
            extra.insert("scan_to_pc".to_owned(), Value::Bool(false));
            extra.insert("scan_to_pc_reason".to_owned(), Value::Str(reason.clone()));
        }
        None => {}
    }

    capabilities
        .extra
        .insert("brother".to_owned(), Value::Dict(extra));
    capabilities
}

/// The device's IPv4 address, if discovery found one.
///
/// `physical_address` first because it is the field discovery put the device's own
/// address in; `address` is a fallback that may still be the raw `device_uri`. A device
/// only known by an mDNS name yields `None` — resolving one is the listener's problem
/// (5.9), not something to guess at here.
fn device_ipv4(scanner: &ScannerInfo) -> Option<Ipv4Addr> {
    let physical = scanner
        .capabilities
        .extra
        .get("brother")
        .and_then(as_dict)
        .and_then(|dict| dict.get("physical_address"))
        .and_then(|value| match value {
            Value::Str(address) => Some(address.as_str()),
            _ => None,
        });

    physical
        .and_then(ipv4)
        .or_else(|| ipv4(&scanner.address))
        .and_then(|address| address.parse().ok())
}

/// The resolutions every eSCL device on this desk has offered, used where a sighting is
/// silent about them.
///
/// `scanimage -A` against the MFC-J5335DW over `sane-airscan` reports
/// `--resolution 100|200|300|600dpi`, and `--mode Color|Gray`. Published as a baseline
/// rather than as a fact about the model: the alternative is an empty list, which a
/// client renders as a scanner that can do nothing.
const ESCL_BASELINE_RESOLUTIONS: &[u32] = &[100, 200, 300, 600];

/// What an eSCL sighting says the device can do.
///
/// The paper paths come from the description when it carries them and default to the
/// glass when it does not — a device with no ADF is the safe assumption, because asking
/// a flatbed for its feeder fails at `sane_start` while asking a feeder for its glass
/// does not.
///
/// Buttons are the *protocol's* four entries, not a per-model claim: the firmware builds
/// its *Scan to PC* menu from `FUNC`, so a Brother that speaks the registration OID has
/// exactly these four and no others ([`skey::register::Function`]). A device that refuses
/// the OID has this reduced to zero by its caller, from the device's own answer.
fn escl_capabilities(sighting: &Sighting) -> Capabilities {
    if !sighting.transport.is_escl() {
        // Seen only through the vendor SANE backend, which scanbus does not use. It is a
        // scanner, and nothing is known about what it can be asked for.
        return Capabilities {
            buttons: panel_buttons(),
            ..Capabilities::default()
        };
    }

    Capabilities {
        resolutions: ESCL_BASELINE_RESOLUTIONS.to_vec(),
        color_modes: vec![ColorMode::Color, ColorMode::Gray],
        sources: acquisition::sources_from_description(&sighting.description)
            .unwrap_or_else(|| vec![Source::Flatbed]),
        // eSCL advertises duplex as a separate ADF source (`ADF Duplex`), which no
        // `scanimage -L` line carries. Claimed only when the description says so.
        duplex: sighting.description.to_ascii_lowercase().contains("duplex"),
        buttons: panel_buttons(),
        ..Capabilities::default()
    }
}

/// The device's physical menu: the four `FUNC` entries, with the firmware's own labels.
fn panel_buttons() -> ButtonsCapability {
    ButtonsCapability {
        count: u32::try_from(Function::ALL.len()).expect("four panel entries fit in a u32"),
        label_configurable: false,
        labels: Function::ALL
            .iter()
            .map(|function| function.device_label().to_owned())
            .collect(),
    }
}

/// The eSCL device a discovered scanner is acquired from, if it has one.
///
/// Read back out of the capability dict rather than passed alongside it, so that the
/// thing `scanbus show --json` prints and the thing `fetch_pages` runs against are the
/// same value by construction.
fn escl_device(scanner: &ScannerInfo) -> Option<EsclDevice> {
    let device_uri = scanner
        .capabilities
        .extra
        .get("brother")
        .and_then(as_dict)
        .and_then(|dict| dict.get("acquisition_uri"))
        .and_then(|value| match value {
            Value::Str(uri) => Some(uri.clone()),
            _ => None,
        })?;

    Some(EsclDevice {
        device_uri,
        resolutions: scanner.capabilities.resolutions.clone(),
        color_modes: scanner.capabilities.color_modes.clone(),
        sources: scanner.capabilities.sources.clone(),
    })
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
    use std::net::SocketAddrV4;
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::ExitStatus;
    use std::sync::Mutex;
    use std::time::Duration;

    use scanbus_core::PairingState;
    use tempfile::TempDir;

    use super::*;

    /// The SANE library path the one dependency lives at on this machine.
    const AIRSCAN_SO: &str = "/usr/lib/x86_64-linux-gnu/sane/libsane-airscan.so.1";

    #[test]
    fn scanimage_lines_are_parsed() {
        let output = r#"
device 'brother4:net1;dev0' is 'Brother MFC-L2710DW'
device 'airscan:escl:Brother MFC-L2710DW http://BRW001122334455.local:80/eSCL/' is a WSD or eSCL Brother MFC-L2710DW series
"#;

        let parsed = parse_scanimage_output(output);
        assert_eq!(parsed.len(), 2);
        // `brother4:` and `brother5:` are one transport now: which of the two `.deb`s a
        // model wanted stopped being a question the day acquisition left them.
        assert_eq!(parsed[0].transport, Transport::Vendor);
        assert_eq!(parsed[0].model.as_deref(), Some("MFC-L2710DW"));
        assert_eq!(parsed[1].transport, Transport::Airscan);
        assert_eq!(
            parsed[1].address_hint.as_deref(),
            Some("brw001122334455.local:80")
        );
    }

    /// The precedence inversion of 5.10, on the two lines the same printer produces.
    ///
    /// Dedup by physical address is unchanged — one scanner comes out — but the survivor
    /// is now the eSCL sighting, because that is the one a scan can be run against.
    #[test]
    fn the_escl_sighting_wins_the_dedup_and_is_what_a_scan_runs_against() {
        let scanners = scanners_from_sightings(
            vec![
                parse_scanimage_line("device 'brother4:net1;dev0' is 'Brother MFC-L2710DW'")
                    .unwrap(),
                parse_scanimage_line("device 'airscan:escl:http://BRW001122334455.local:80/eSCL/' is 'Brother MFC-L2710DW series'")
                    .unwrap(),
            ],
            InstalledState {
                airscan: Presence::Present,
            },
            &never_asked(),
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
            brother.get("transport"),
            Some(&Value::Str("airscan".to_owned()))
        );
        // The acquisition choice, visible on the bus rather than only in this crate.
        assert_eq!(
            brother.get("acquisition_uri"),
            Some(&Value::Str(
                "airscan:escl:http://BRW001122334455.local:80/eSCL/".to_owned()
            ))
        );
        assert_eq!(brother.get("airscan_installed"), Some(&Value::Bool(true)));
        // Nothing about a vendor driver survives on the wire.
        assert!(!brother.contains_key("driver"), "{brother:?}");
        assert!(!brother.contains_key("driver_installed"), "{brother:?}");
        assert_eq!(scanners[0].capabilities.buttons.count, 4);
    }

    /// A model nobody wrote a table entry for, which is every model but five.
    ///
    /// The old per-model table published this scanner with no resolutions, no sources and
    /// no buttons. What it can do now comes from the sighting and from the panel protocol,
    /// neither of which needs to have heard of the model.
    #[test]
    fn an_unknown_model_is_described_from_its_sighting_rather_than_from_a_table() {
        let scanners = scanners_from_sightings(
            vec![
                parse_scanimage_line(
                    "device 'escl:http://192.168.1.3:80' is a Brother MFC-J5335DW adf,platen scanner",
                )
                .unwrap(),
            ],
            InstalledState::default(),
            &never_asked(),
        )
        .unwrap();

        assert_eq!(scanners.len(), 1);
        let capabilities = &scanners[0].capabilities;
        assert_eq!(capabilities.resolutions, vec![100, 200, 300, 600]);
        assert_eq!(capabilities.color_modes, vec![ColorMode::Color, ColorMode::Gray]);
        // Read off `adf,platen` in the description, not out of a model name.
        assert_eq!(capabilities.sources, vec![Source::Flatbed, Source::Adf]);
        assert!(!capabilities.duplex);
        // The firmware's four `FUNC` entries, with the firmware's labels.
        assert_eq!(capabilities.buttons.count, 4);
        assert_eq!(capabilities.buttons.labels[0], "Scan to File");
        assert!(!capabilities.buttons.label_configurable);
    }

    /// The degraded case of the design's §4: no eSCL anywhere, so the scanner is still
    /// discovered — the alternative is a printer that is simply not there — but nothing
    /// claims a scan can be run against it.
    #[test]
    fn a_device_seen_only_through_the_vendor_backend_is_still_discovered() {
        let scanners = scanners_from_sightings(
            vec![
                parse_scanimage_line("device 'brother4:net1;dev0' is 'Brother MFC-L2710DW'")
                    .unwrap(),
            ],
            InstalledState::default(),
            &never_asked(),
        )
        .unwrap();

        assert_eq!(scanners.len(), 1);
        let brother = scanners[0]
            .capabilities
            .extra
            .get("brother")
            .and_then(as_dict)
            .unwrap();
        assert_eq!(
            brother.get("transport"),
            Some(&Value::Str("vendor".to_owned()))
        );
        assert!(!brother.contains_key("acquisition_uri"), "{brother:?}");
        assert_eq!(escl_device(&scanners[0]), None);
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
        /// What `scanimage -L` prints. Empty means "no devices", not "no scanimage".
        scanimage: &'static str,
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

            if program.ends_with("scanimage") {
                return Ok(Output {
                    status: ExitStatus::from_raw(0),
                    stdout: self.scanimage.as_bytes().to_vec(),
                    stderr: Vec::new(),
                });
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

    /// The MFC-J5335DW of this machine over eSCL, as discovery hands it to
    /// `ensure_installed`.
    fn brother_mfc() -> ScannerInfo {
        scanners_from_sightings(
            vec![
                parse_scanimage_line(
                    "device 'escl:http://192.168.1.3:80' is a Brother MFC-J5335DW adf,platen scanner",
                )
                .unwrap(),
            ],
            InstalledState::default(),
            &never_asked(),
        )
        .unwrap()
        .remove(0)
    }

    /// The same printer on a machine where nothing speaks eSCL to it.
    fn vendor_only_mfc() -> ScannerInfo {
        scanners_from_sightings(
            vec![
                parse_scanimage_line("device 'brother4:net1;dev0' is 'Brother MFC-L2710DW'")
                    .unwrap(),
            ],
            InstalledState::default(),
            &never_asked(),
        )
        .unwrap()
        .remove(0)
    }

    /// The state every scanner is in before anybody pairs it: nothing has been asked, so
    /// nothing the device said is on record.
    fn never_asked() -> BTreeMap<ScannerId, ScanToPc> {
        BTreeMap::new()
    }

    // ------------------------------------------------------ the scan-to-PC degraded path

    /// A device on the network that answers the scan-key OID however it is told to.
    ///
    /// The fake SNMP responder the acceptance criterion asks for: it is what lets "a
    /// model that refuses is still a scanner" be a test rather than a hardware ritual.
    #[derive(Debug)]
    struct FakeSnmp {
        /// `None` is a printer that is switched off.
        answer: Option<skey::snmp::ErrorStatus>,
        asked: Mutex<Vec<SocketAddrV4>>,
    }

    impl FakeSnmp {
        fn answering(answer: Option<skey::snmp::ErrorStatus>) -> Arc<Self> {
            Arc::new(Self {
                answer,
                asked: Mutex::new(Vec::new()),
            })
        }

        fn asked(&self) -> Vec<SocketAddrV4> {
            self.asked.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SnmpTransport for FakeSnmp {
        async fn exchange(
            &self,
            device: SocketAddrV4,
            request: &skey::snmp::Message,
        ) -> Result<skey::snmp::Message, registrar::TransportError> {
            use skey::snmp::{Message, Pdu, PduKind, Value as SnmpValue, VarBind, Version};

            self.asked.lock().unwrap().push(device);
            let Some(error_status) = self.answer else {
                return Err(registrar::TransportError::Timeout(
                    registrar::RESPONSE_TIMEOUT,
                ));
            };

            Ok(Message {
                version: Version::V1,
                community: request.community.clone(),
                pdu: Pdu {
                    kind: PduKind::Response,
                    request_id: request.pdu.request_id,
                    error_status,
                    error_index: 0,
                    varbinds: vec![VarBind::new(
                        request.pdu.varbinds[0].oid.clone(),
                        SnmpValue::OctetString(b"TRUE".to_vec()),
                    )],
                },
            })
        }
    }

    /// One eSCL sighting of a four-button model, at an address SNMP can be sent to.
    const NETWORK_MFC: &str =
        "device 'escl:http://192.168.1.23:80' is a Brother MFC-L2710DW flatbed scanner\n";

    fn networked_backend(
        root: &TempDir,
        transport: Arc<dyn SnmpTransport>,
    ) -> (BrotherBackend, Arc<RecordingRunner>) {
        let runner = Arc::new(RecordingRunner {
            scanimage: NETWORK_MFC,
            ..RecordingRunner::default()
        });
        let scanimage = root.path().join("scanimage");
        fs::write(&scanimage, b"stub").unwrap();

        let backend = BrotherBackend {
            scanimage_path: scanimage,
            transport,
            ..backend(root, Arc::clone(&runner))
        };
        (backend, runner)
    }

    fn brother_dict(scanner: &ScannerInfo) -> BTreeMap<String, Value> {
        scanner
            .capabilities
            .extra
            .get("brother")
            .and_then(as_dict)
            .cloned()
            .expect("every Brother scanner carries its dict")
    }

    /// The degraded path of the design's §4, end to end: the device says it does not
    /// know the OID, `Pair()` still succeeds, and the scanner keeps everything except
    /// its buttons.
    #[tokio::test]
    async fn a_device_that_refuses_the_oid_keeps_a_scanner_and_loses_its_buttons() {
        let root = sysroot(&[AIRSCAN_SO, "/usr/bin/brscan-skey"]);
        let device = FakeSnmp::answering(Some(skey::snmp::ErrorStatus::NoSuchName));
        let (backend, _) = networked_backend(&root, device.clone());

        // Before anybody asks, the model table's guess stands.
        let before = backend.discover().await.unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].capabilities.buttons.count, 4);

        // Pairing asks — and does not fail because the answer was no.
        let (outcome, progress) = ensure_installed_progress(&backend, &before[0]).await;
        outcome.expect("a model without scan-to-PC is still pairable");
        assert_eq!(progress.last(), Some(&PairingProgress::Ready));

        let asked = device.asked();
        assert_eq!(
            asked,
            vec![SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 23), 161)],
            "asked exactly the device, exactly once",
        );

        // And discovery now reports what the device said about itself.
        let after = backend.discover().await.unwrap();
        assert_eq!(after.len(), 1, "the scanner is still discovered");
        assert_eq!(after[0].id, before[0].id);
        assert_eq!(after[0].status, Status::Online);
        assert_eq!(
            after[0].capabilities.buttons.count, 0,
            "the device's own answer beats the model table",
        );

        let dict = brother_dict(&after[0]);
        assert_eq!(dict.get("scan_to_pc"), Some(&Value::Bool(false)));
        let Some(Value::Str(reason)) = dict.get("scan_to_pc_reason") else {
            panic!("the reason must travel with the scanner: {dict:?}");
        };
        assert!(
            reason.contains("does not appear to support scan-to-PC"),
            "{reason}",
        );
        // Everything the pull-scanning path needs is untouched.
        assert_eq!(
            dict.get("device_uri"),
            Some(&Value::Str("escl:http://192.168.1.23:80".to_owned()))
        );
        assert_eq!(
            after[0].capabilities.sources,
            before[0].capabilities.sources
        );
        assert_eq!(
            after[0].capabilities.resolutions,
            before[0].capabilities.resolutions
        );
    }

    #[tokio::test]
    async fn a_device_that_knows_the_oid_keeps_its_buttons() {
        let root = sysroot(&[AIRSCAN_SO, "/usr/bin/brscan-skey"]);
        let device = FakeSnmp::answering(Some(skey::snmp::ErrorStatus::NoError));
        let (backend, _) = networked_backend(&root, device);

        let scanner = backend.discover().await.unwrap().remove(0);
        ensure_installed_progress(&backend, &scanner)
            .await
            .0
            .unwrap();

        let after = backend.discover().await.unwrap().remove(0);
        assert_eq!(after.capabilities.buttons.count, 4);
        assert_eq!(
            brother_dict(&after).get("scan_to_pc"),
            Some(&Value::Bool(true))
        );
    }

    /// A printer that was switched off during pairing has said nothing about itself, and
    /// must not be recorded as a model without buttons — that would need another pairing
    /// to undo.
    #[tokio::test]
    async fn a_printer_that_does_not_answer_is_not_recorded_as_button_less() {
        let root = sysroot(&[AIRSCAN_SO, "/usr/bin/brscan-skey"]);
        let device = FakeSnmp::answering(None);
        let (backend, _) = networked_backend(&root, device);

        let scanner = backend.discover().await.unwrap().remove(0);
        ensure_installed_progress(&backend, &scanner)
            .await
            .0
            .unwrap();

        let after = backend.discover().await.unwrap().remove(0);
        assert_eq!(after.capabilities.buttons.count, 4);
        let dict = brother_dict(&after);
        assert!(
            !dict.contains_key("scan_to_pc"),
            "silence is not an answer to record: {dict:?}",
        );
    }

    /// A USB scanner cannot be registered by this backend at all. Saying so is more
    /// useful than leaving four keys advertised that nothing will ever deliver.
    #[tokio::test]
    async fn a_usb_scanner_reports_no_panel_keys_and_says_why() {
        let root = sysroot(&[AIRSCAN_SO, "/usr/bin/brscan-skey"]);
        let device = FakeSnmp::answering(Some(skey::snmp::ErrorStatus::NoError));
        let runner = Arc::new(RecordingRunner {
            scanimage: "device 'brother4:bus4;dev1:usb:001:002' is a Brother MFC-L2710DW\n",
            ..RecordingRunner::default()
        });
        let scanimage = root.path().join("scanimage");
        fs::write(&scanimage, b"stub").unwrap();
        let backend = BrotherBackend {
            scanimage_path: scanimage,
            transport: device.clone(),
            ..backend(&root, runner)
        };

        let scanner = backend.discover().await.unwrap().remove(0);
        // The scan-to-PC question directly rather than through `ensure_installed`: this
        // device is only visible over `brother4:`, so pairing now stops at the eSCL check
        // (`a_device_with_no_escl_fails_pairing_and_says_why`) before it ever gets here.
        // What is under test is the panel, not acquisition.
        backend.note_scan_to_pc(&scanner).await;

        assert!(
            device.asked().is_empty(),
            "there is nothing to send an SNMP datagram to",
        );

        let after = backend.discover().await.unwrap().remove(0);
        assert_eq!(after.capabilities.buttons.count, 0);
        let Some(Value::Str(reason)) = brother_dict(&after).get("scan_to_pc_reason").cloned()
        else {
            panic!("a USB scanner must say why it has no keys");
        };
        assert!(reason.contains("USB"), "{reason}");
    }

    #[test]
    fn a_device_address_is_read_from_discovery_metadata_or_not_at_all() {
        let networked = scanners_from_sightings(
            vec![parse_scanimage_line(NETWORK_MFC).unwrap()],
            InstalledState::default(),
            &never_asked(),
        )
        .unwrap()
        .remove(0);
        assert_eq!(
            device_ipv4(&networked),
            Some(Ipv4Addr::new(192, 168, 1, 23))
        );

        // A vendor URI names a SANE device, not an address, and an mDNS name is not one
        // this backend resolves. Neither may be guessed at.
        assert_eq!(device_ipv4(&vendor_only_mfc()), None);
    }

    // -------------------------------------------------------------- the panel listener

    /// The same four-button model, at a loopback address a test can send a datagram *from*.
    const LOOPBACK_MFC: &str =
        "device 'escl:http://127.0.0.2:80' is a Brother MFC-L2710DW adf,platen scanner\n";

    /// A backend whose listener asks for `port` instead of the fixed UDP/54925, which no
    /// test may bind: it is the port the machine running the suite may well have
    /// `brscan-skey` on, and two tests could not share it either.
    fn listening_backend(
        root: &TempDir,
        scanimage: &'static str,
        port: u16,
    ) -> (BrotherBackend, Arc<FakeSnmp>) {
        let transport = FakeSnmp::answering(Some(skey::snmp::ErrorStatus::NoError));
        let runner = Arc::new(RecordingRunner {
            scanimage,
            ..RecordingRunner::default()
        });
        let scanimage_path = root.path().join("scanimage");
        fs::write(&scanimage_path, b"stub").unwrap();

        let backend = BrotherBackend {
            scanimage_path,
            transport: transport.clone(),
            listener: Arc::new(Listener::new(port)),
            ..backend(root, runner)
        };
        (backend, transport)
    }

    /// A free port from below the ephemeral range, so that no socket another test binds with
    /// port 0 can land on the one this test's listener is about to ask for. See
    /// [`listener`]'s own `free_port` for the longer version of why port 0 is not usable
    /// here.
    async fn free_port() -> u16 {
        static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(25_925);

        for _ in 0..64 {
            let port = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let probe =
                tokio::net::UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)).await;
            if probe.is_ok() {
                drop(probe);
                return port;
            }
        }
        panic!("no free port for this test in 64 tries");
    }

    async fn press(from: Ipv4Addr, port: u16, function: skey::register::Function) {
        let payload = format!(
            "TYPE=BR;BUTTON=SCAN;USER=\"desktop\";FUNC={};HOST=127.0.0.1:{port};APPNUM={};\
             CLIENT={from}",
            function.as_str(),
            function.appnum(),
        );
        let datagram = skey::event::Frame {
            id: 0x01,
            code: 0x01,
            payload: &payload,
        }
        .to_datagram();

        let socket = tokio::net::UdpSocket::bind(SocketAddrV4::new(from, 0))
            .await
            .unwrap();
        socket
            .send_to(&datagram, SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
    }

    async fn next_trigger(stream: &mut BoxStream<'static, ScanTrigger>) -> ScanTrigger {
        tokio::time::timeout(
            Duration::from_secs(5),
            std::future::poll_fn(|cx| {
                futures_core::Stream::poll_next(std::pin::Pin::new(&mut *stream), cx)
            }),
        )
        .await
        .expect("a press should arrive")
        .expect("the stream must not end")
    }

    /// A press on a paired scanner's panel, from `Connect()` to a trigger with the button
    /// index of the entry that was chosen.
    #[tokio::test]
    async fn a_paired_scanner_reports_its_panel_presses_as_button_triggers() {
        // `brscan-skey`'s files on disk deliberately: 5.10 dropped it from the dependency
        // set, and its being present must change nothing. Files being there is not the
        // same thing as the daemon running, which is what would take the port.
        let root = sysroot(&[AIRSCAN_SO, "/usr/bin/brscan-skey"]);
        let port = free_port().await;
        let (backend, _) = listening_backend(&root, LOOPBACK_MFC, port);

        let scanner = backend.discover().await.unwrap().remove(0);
        ensure_installed_progress(&backend, &scanner)
            .await
            .0
            .unwrap();

        let mut stream = backend.start_listening(&scanner).await.unwrap();
        press(
            Ipv4Addr::new(127, 0, 0, 2),
            port,
            skey::register::Function::Ocr,
        )
        .await;

        let trigger = next_trigger(&mut stream).await;
        assert_eq!(trigger.scanner_id, scanner.id);
        assert_eq!(trigger.kind, scanbus_core::TriggerKind::Button { index: 2 });
        assert!(
            !trigger.id.is_empty(),
            "a trigger `fetch_pages` can be called with needs an id",
        );

        // And `Disconnect()` gives the port back, so the next `Connect()` can have it.
        backend.stop_listening(&scanner.id).await.unwrap();
        assert_eq!(backend.listener.port(), None);
        let again = backend.start_listening(&scanner).await.unwrap();
        assert_eq!(backend.listener.port(), Some(port));
        // Dropping the stream is the other half of the contract: it releases the port too,
        // without a `stop_listening`.
        drop(again);
        backend.stop_listening(&scanner.id).await.unwrap();
        assert_eq!(backend.listener.port(), None);
    }

    /// `brscan-skey` holds the port first: the error names it, and the scanner is left
    /// alone — still discovered, still reporting its keys, nothing sent to the device.
    #[tokio::test]
    async fn a_port_brscan_skey_holds_is_refused_by_name() {
        let root = sysroot(&[AIRSCAN_SO, "/usr/bin/brscan-skey"]);
        let squatter = tokio::net::UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
            .await
            .unwrap();
        let port = squatter.local_addr().unwrap().port();
        let (backend, _) = listening_backend(&root, LOOPBACK_MFC, port);

        let scanner = backend.discover().await.unwrap().remove(0);
        let Err(error) = backend.start_listening(&scanner).await else {
            panic!("the port is taken, so there is no listening to be done");
        };
        let message = error.to_string();
        assert!(
            message.contains("brscan-skey") && message.contains("brscan-skey -t"),
            "the one likely cause must be named, with what to do about it: {message}",
        );
        assert!(
            message.contains(&port.to_string()) && message.contains("scanned from the host"),
            "and the port, and what still works: {message}",
        );

        // The scanner itself is untouched: discovery still reports it, with its keys.
        let after = backend.discover().await.unwrap().remove(0);
        assert_eq!(after.id, scanner.id);
        assert_eq!(after.capabilities.buttons.count, 4);
    }

    /// The same failure on a machine with no `brscan-skey`: naming it as the culprit would
    /// send the user hunting a package that is not there.
    #[tokio::test]
    async fn a_port_taken_by_something_else_says_how_to_find_out_what() {
        let root = sysroot(&[AIRSCAN_SO]);
        let squatter = tokio::net::UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
            .await
            .unwrap();
        let port = squatter.local_addr().unwrap().port();
        let (backend, _) = listening_backend(&root, LOOPBACK_MFC, port);

        let scanner = backend.discover().await.unwrap().remove(0);
        let Err(error) = backend.start_listening(&scanner).await else {
            panic!("the port is taken");
        };
        let message = error.to_string();
        assert!(
            message.contains("does not appear to be installed") && message.contains("ss -lunp"),
            "{message}",
        );
    }

    /// A USB scanner and a model that refused the OID both connect, and neither binds the
    /// port: pairing's last step is `start_listening`, so an error here would make a
    /// perfectly good pull scanner unpairable.
    #[tokio::test]
    async fn a_scanner_with_no_panel_presses_to_deliver_still_connects() {
        let usb = "device 'brother4:bus4;dev1:usb:001:002' is a Brother MFC-L2710DW\n";
        for (case, scanimage, answer, pairs) in [
            // A `brother4:` USB device has no eSCL and so cannot be scanned from at all
            // any more; it is here for the listener half, which still has to hand it an
            // open, silent stream rather than an error.
            ("usb", usb, skey::snmp::ErrorStatus::NoError, false),
            (
                "refused the OID",
                LOOPBACK_MFC,
                skey::snmp::ErrorStatus::NoSuchName,
                true,
            ),
        ] {
            let root = sysroot(&[AIRSCAN_SO, "/usr/bin/brscan-skey"]);
            let port = free_port().await;
            let (mut backend, _) = listening_backend(&root, scanimage, port);
            backend.transport = FakeSnmp::answering(Some(answer));

            let scanner = backend.discover().await.unwrap().remove(0);
            if pairs {
                ensure_installed_progress(&backend, &scanner)
                    .await
                    .0
                    .unwrap_or_else(|error| panic!("{case} must still pair: {error}"));
            } else {
                backend.note_scan_to_pc(&scanner).await;
            }

            let mut stream = backend
                .start_listening(&scanner)
                .await
                .unwrap_or_else(|error| panic!("{case} must still connect: {error}"));
            assert_eq!(
                backend.listener.port(),
                None,
                "{case} has nothing to listen for and must not hold the port",
            );

            // Open, not ended: the daemon reads an ended stream as a listener that died and
            // retries it until the scanner lands in `Status="error"`.
            assert!(
                tokio::time::timeout(Duration::from_millis(50), next_trigger(&mut stream))
                    .await
                    .is_err(),
                "{case} must be handed a stream that stays open and says nothing",
            );

            backend.stop_listening(&scanner.id).await.unwrap();
        }
    }

    /// `stop_listening` on something that never listened, and twice over.
    #[tokio::test]
    async fn stopping_a_listener_that_is_not_running_is_not_an_error() {
        let root = sysroot(&[AIRSCAN_SO]);
        let port = free_port().await;
        let (backend, _) = listening_backend(&root, LOOPBACK_MFC, port);
        let scanner = backend.discover().await.unwrap().remove(0);

        backend.stop_listening(&scanner.id).await.unwrap();
        let stream = backend.start_listening(&scanner).await.unwrap();
        backend.stop_listening(&scanner.id).await.unwrap();
        drop(stream);
        backend.stop_listening(&scanner.id).await.unwrap();
        assert_eq!(backend.listener.port(), None);
    }


    // ----------------------------------------------------------------- acquisition

    /// A stand-in for the packaged `scanbus-scanimage`: records its argv and writes one
    /// page.
    ///
    /// The argv is the assertion. Everything else about acquisition — the batching, the
    /// partial-PNM detection, the end of feed — belongs to `scanbus-backend-common` and
    /// is tested there; what this crate decides is *which device and which options*, and
    /// that is exactly what lands in this file.
    fn recording_scanimage(root: &TempDir) -> (PathBuf, PathBuf) {
        let argv = root.path().join("scanimage-argv");
        let script = root.path().join("scanimage-helper.sh");
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
printf 'P5\\n1 1\\n255\\n\\001' > \"$(printf \"$batch\" 1)\"\n",
                argv = argv.display(),
            ),
        )
        .unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).unwrap();
        (script, argv)
    }

    fn recorded_argv(path: &Path) -> Vec<String> {
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    async fn next_page(
        stream: &mut BoxStream<'static, Result<RawPage, BackendError>>,
    ) -> Option<Result<RawPage, BackendError>> {
        tokio::time::timeout(
            Duration::from_secs(5),
            std::future::poll_fn(|cx| {
                futures_core::Stream::poll_next(std::pin::Pin::new(&mut *stream), cx)
            }),
        )
        .await
        .expect("acquisition should not hang")
    }

    /// A backend that will actually scan: a listener on a test port and a `scanimage`
    /// helper that records what it was asked for.
    async fn acquiring_backend(root: &TempDir) -> (BrotherBackend, ScannerInfo, PathBuf) {
        let port = free_port().await;
        let (mut backend, _) = listening_backend(root, LOOPBACK_MFC, port);
        let (helper, argv) = recording_scanimage(root);
        backend.scanimage_helper_path = helper;

        let scanner = backend.discover().await.unwrap().remove(0);
        ensure_installed_progress(&backend, &scanner)
            .await
            .0
            .unwrap();
        (backend, scanner, argv)
    }

    /// One press of a key assigned the `document` profile, from datagram to page.
    ///
    /// The assertion is the command line, because that is the whole of what this issue
    /// decides: the eSCL device name discovery recorded, the feeder the profile implies,
    /// and a resolution from the device's own list — with no vendor URI anywhere in it.
    #[tokio::test]
    async fn a_press_scans_over_escl_with_the_options_the_assigned_profile_implies() {
        let root = sysroot(&[AIRSCAN_SO]);
        let (backend, scanner, argv) = acquiring_backend(&root).await;

        backend
            .set_button_mapping(
                &scanner.id,
                Function::File.button_index(),
                Some(ProfileKind::Document),
                &BTreeMap::new(),
            )
            .await
            .unwrap();

        let mut stream = backend.start_listening(&scanner).await.unwrap();
        press(
            Ipv4Addr::new(127, 0, 0, 2),
            backend.listener.port().unwrap(),
            Function::File,
        )
        .await;
        let trigger = next_trigger(&mut stream).await;

        let mut pages = backend.fetch_pages(&scanner.id, &trigger.id).await.unwrap();
        let page = next_page(&mut pages).await.unwrap().unwrap();
        assert_eq!(page.index, 0);
        assert_eq!(page.resolution_dpi, 300);
        assert!(next_page(&mut pages).await.is_none());

        let argv = recorded_argv(&argv);
        assert!(
            argv.contains(&"--device-name=escl:http://127.0.0.2:80".to_owned()),
            "the eSCL device discovery recorded, and nothing else: {argv:?}",
        );
        assert!(argv.contains(&"--source=ADF".to_owned()), "{argv:?}");
        assert!(argv.contains(&"--resolution=300".to_owned()), "{argv:?}");
        assert!(
            !argv.iter().any(|arg| arg.contains("brother4")
                || arg.contains("brother5")
                || arg.contains("brscan")),
            "no vendor driver may appear on the command line: {argv:?}",
        );
    }

    /// The glass, and the one thing that keeps a flatbed run from being an open-ended
    /// batch.
    #[tokio::test]
    async fn an_image_profile_scans_one_sheet_from_the_glass() {
        let root = sysroot(&[AIRSCAN_SO]);
        let (backend, scanner, argv) = acquiring_backend(&root).await;

        backend
            .set_button_mapping(
                &scanner.id,
                Function::Image.button_index(),
                Some(ProfileKind::Image),
                &BTreeMap::new(),
            )
            .await
            .unwrap();

        let mut stream = backend.start_listening(&scanner).await.unwrap();
        press(
            Ipv4Addr::new(127, 0, 0, 2),
            backend.listener.port().unwrap(),
            Function::Image,
        )
        .await;
        let trigger = next_trigger(&mut stream).await;
        let mut pages = backend.fetch_pages(&scanner.id, &trigger.id).await.unwrap();
        next_page(&mut pages).await.unwrap().unwrap();

        let argv = recorded_argv(&argv);
        assert!(argv.contains(&"--source=Flatbed".to_owned()), "{argv:?}");
        assert!(argv.contains(&"--batch-count=1".to_owned()), "{argv:?}");
    }

    /// The trait's "callable exactly once per `trigger_id`", which here is what stops a
    /// second `Job1` from pulling a second sheet through the feeder.
    #[tokio::test]
    async fn a_trigger_can_be_fetched_once_and_an_unknown_one_never() {
        let root = sysroot(&[AIRSCAN_SO]);
        let (backend, scanner, _) = acquiring_backend(&root).await;

        let mut stream = backend.start_listening(&scanner).await.unwrap();
        press(
            Ipv4Addr::new(127, 0, 0, 2),
            backend.listener.port().unwrap(),
            Function::Ocr,
        )
        .await;
        let trigger = next_trigger(&mut stream).await;

        backend.fetch_pages(&scanner.id, &trigger.id).await.unwrap();

        for (case, id) in [("already fetched", trigger.id.as_str()), ("never minted", "press-99")] {
            match backend.fetch_pages(&scanner.id, id).await {
                Err(BackendError::UnknownJob { scanner: reported, job }) => {
                    assert_eq!(reported, scanner.id, "{case}");
                    assert_eq!(job, id, "{case}");
                }
                Err(other) => panic!("{case} must be UnknownJob, got {other:?}"),
                Ok(_) => panic!("{case} must not start a second scan"),
            }
        }
    }

    /// A press with nothing assigned to the key still scans: the paper is already in the
    /// feeder, and the daemon is the one that decides what to do with the pages.
    #[tokio::test]
    async fn an_unassigned_key_still_acquires_at_the_devices_defaults() {
        let root = sysroot(&[AIRSCAN_SO]);
        let (backend, scanner, argv) = acquiring_backend(&root).await;

        let mut stream = backend.start_listening(&scanner).await.unwrap();
        press(
            Ipv4Addr::new(127, 0, 0, 2),
            backend.listener.port().unwrap(),
            Function::Email,
        )
        .await;
        let trigger = next_trigger(&mut stream).await;

        let mut pages = backend.fetch_pages(&scanner.id, &trigger.id).await.unwrap();
        next_page(&mut pages).await.unwrap().unwrap();

        let argv = recorded_argv(&argv);
        assert!(argv.contains(&"--source=Flatbed".to_owned()), "{argv:?}");
    }

    /// Clearing a key removes what it was mapped to, rather than leaving last week's
    /// assignment driving the scan.
    #[tokio::test]
    async fn clearing_a_key_forgets_its_options() {
        let root = sysroot(&[AIRSCAN_SO]);
        let (backend, scanner, argv) = acquiring_backend(&root).await;
        let index = Function::File.button_index();

        backend
            .set_button_mapping(&scanner.id, index, Some(ProfileKind::Document), &BTreeMap::new())
            .await
            .unwrap();
        backend
            .set_button_mapping(&scanner.id, index, None, &BTreeMap::new())
            .await
            .unwrap();

        let mut stream = backend.start_listening(&scanner).await.unwrap();
        press(
            Ipv4Addr::new(127, 0, 0, 2),
            backend.listener.port().unwrap(),
            Function::File,
        )
        .await;
        let trigger = next_trigger(&mut stream).await;
        let mut pages = backend.fetch_pages(&scanner.id, &trigger.id).await.unwrap();
        next_page(&mut pages).await.unwrap().unwrap();

        let argv = recorded_argv(&argv);
        assert!(
            argv.contains(&"--source=Flatbed".to_owned()),
            "a cleared key must not keep asking for the feeder: {argv:?}",
        );
    }

    /// The panel has four entries and the API's indices are 0..=3, the same table
    /// registration and event decoding are written against.
    #[tokio::test]
    async fn a_button_index_the_panel_does_not_have_is_refused() {
        let backend = BrotherBackend::default();
        let scanner = brother_mfc();

        let error = backend
            .set_button_mapping(&scanner.id, 4, Some(ProfileKind::Image), &BTreeMap::new())
            .await
            .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("button 4"), "{message}");
        assert!(message.contains("0..=3"), "{message}");
    }

    /// A scanner nothing has been discovered for cannot be scanned from, and the message
    /// has to say that rediscovering is the fix — not that something needs installing.
    #[tokio::test]
    async fn a_scanner_with_no_recorded_escl_device_is_not_reachable() {
        let root = sysroot(&[AIRSCAN_SO]);
        let (backend, scanner, _) = acquiring_backend(&root).await;

        let mut stream = backend.start_listening(&scanner).await.unwrap();
        press(
            Ipv4Addr::new(127, 0, 0, 2),
            backend.listener.port().unwrap(),
            Function::File,
        )
        .await;
        let trigger = next_trigger(&mut stream).await;
        backend.escl.lock().unwrap().clear();

        let Err(error) = backend.fetch_pages(&scanner.id, &trigger.id).await else {
            panic!("a scanner with no eSCL device on record must not scan");
        };

        let BackendError::NotReachable { detail, .. } = &error else {
            panic!("expected NotReachable, got {error:?}");
        };
        assert!(detail.contains("discovery"), "{detail}");
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

    /// The boring case: `sane-airscan` present, so pairing walks straight through — but
    /// still through `installing_backend`, because that is the state a client's progress
    /// UI is written against and it must not change when a dependency set does.
    ///
    /// `brscan-skey` is on this sysroot deliberately: it must make no difference either
    /// way, and a step naming it here would be a dependency that came back.
    #[tokio::test]
    async fn present_dependencies_still_pass_through_installing_backend() {
        let root = sysroot(&[AIRSCAN_SO, "/usr/bin/brscan-skey"]);
        let runner = Arc::new(RecordingRunner::default());
        let backend = backend(&root, Arc::clone(&runner));

        let (outcome, progress) = ensure_installed_progress(&backend, &brother_mfc()).await;

        outcome.unwrap();
        assert_eq!(
            progress,
            vec![
                PairingProgress::Checking {
                    message: "checking Brother dependencies for MFC-J5335DW".to_owned(),
                },
                PairingProgress::Installing {
                    package: "sane-airscan".to_owned(),
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

    /// The acceptance criterion of 5.10, as a test: with `sane-airscan` gone the message
    /// names it and a command that works, and says nothing about Brother's website.
    #[tokio::test]
    async fn a_missing_airscan_is_refused_by_name_and_by_the_command_that_fixes_it() {
        let root = sysroot(&["/usr/bin/brscan-skey"]);
        let runner = Arc::new(RecordingRunner::default());
        let backend = backend(&root, Arc::clone(&runner));

        let (outcome, progress) = ensure_installed_progress(&backend, &brother_mfc()).await;

        let error = outcome.unwrap_err();
        let BackendError::InstallFailed { package, detail } = &error else {
            panic!("expected an install failure, got {error:?}");
        };
        assert_eq!(package, "sane-airscan");
        assert!(detail.contains("apt install sane-airscan"), "{detail}");
        assert!(detail.contains("scanbus does not install it"), "{detail}");
        assert!(
            !detail.to_ascii_lowercase().contains("brother.com"),
            "the user is sent to apt, not to a vendor download form: {detail}"
        );

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
    }

    /// A Brother that only ever appeared as `brother4:`. Installing something is not the
    /// fix, and the message must not pretend it is — but it still names the package the
    /// user is most likely missing, because on this path it usually is.
    #[tokio::test]
    async fn a_device_with_no_escl_fails_pairing_and_says_why() {
        let root = sysroot(&[AIRSCAN_SO]);
        let runner = Arc::new(RecordingRunner::default());
        let backend = backend(&root, Arc::clone(&runner));

        let (outcome, progress) = ensure_installed_progress(&backend, &vendor_only_mfc()).await;

        let BackendError::InstallFailed { package, detail } = outcome.unwrap_err() else {
            panic!("expected an install failure");
        };
        assert_eq!(package, "sane-airscan");
        assert!(detail.contains("not over eSCL"), "{detail}");
        assert!(detail.contains("MFC-L2710DW"), "{detail}");
        assert!(detail.contains("brscan4/brscan5"), "{detail}");
        // The dependency step still ran and still passed: what failed is the device.
        assert!(
            progress
                .iter()
                .any(|step| matches!(step, PairingProgress::Installing { package, .. }
                    if package == "sane-airscan")),
            "{progress:?}"
        );
    }

    /// A package can be installed and its files removed, and the status database keeps
    /// saying `installed`. Believing it defers the failure to `start_listening`, where
    /// it looks like a SANE problem.
    #[tokio::test]
    async fn a_registered_package_with_no_files_is_not_installed() {
        let root = sysroot(&[]);
        let runner = Arc::new(RecordingRunner {
            registered: BTreeSet::from(["sane-airscan"]),
            files: BTreeMap::from([(
                "sane-airscan",
                vec!["/usr/share/doc/sane-airscan/copyright".to_owned()],
            )]),
            ..RecordingRunner::default()
        });
        let backend = backend(&root, Arc::clone(&runner));

        let (outcome, _) = ensure_installed_progress(&backend, &brother_mfc()).await;

        let BackendError::InstallFailed { package, detail } = outcome.unwrap_err() else {
            panic!("expected an install failure");
        };
        assert_eq!(package, "sane-airscan");
        assert!(detail.contains("none of its files are on disk"), "{detail}");
        assert!(detail.contains("reinstall"), "{detail}");
    }

    /// The SANE library path is architecture-dependent, so the hard-coded list is a
    /// fast path, not the answer. The package's own file list is what settles it.
    #[tokio::test]
    async fn a_backend_installed_outside_the_known_paths_is_found_through_the_package() {
        let elsewhere = "/usr/lib/riscv64-linux-gnu/sane/libsane-airscan.so.1";
        let root = sysroot(&[elsewhere]);
        let runner = Arc::new(RecordingRunner {
            registered: BTreeSet::from(["sane-airscan"]),
            files: BTreeMap::from([(
                "sane-airscan",
                vec![
                    "/usr/share/doc/sane-airscan".to_owned(),
                    elsewhere.to_owned(),
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
        let root = sysroot(&[]);
        let runner = Arc::new(RecordingRunner {
            registered: BTreeSet::from(["sane-airscan"]),
            files: BTreeMap::from([("sane-airscan", vec![AIRSCAN_SO.to_owned()])]),
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

        // The library was never there; the cancelled run cannot have created it, and the
        // second run has to reach its verdict from the same evidence a first one would.
        assert!(!root.path().join(AIRSCAN_SO.trim_start_matches('/')).exists());
        let library = root.path().join(AIRSCAN_SO.trim_start_matches('/'));
        fs::create_dir_all(library.parent().unwrap()).unwrap();
        fs::write(&library, b"stub").unwrap();

        let (outcome, progress) = ensure_installed_progress(&backend, &scanner).await;

        outcome.unwrap();
        assert_eq!(
            progress,
            vec![
                PairingProgress::Checking {
                    message: "checking Brother dependencies for MFC-J5335DW".to_owned(),
                },
                PairingProgress::Installing {
                    package: "sane-airscan".to_owned(),
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
    ///
    /// **Re-justified for 5.10.** The allowlist is unchanged and the argument for it is
    /// stronger, not weaker: the crate now runs `scanimage` for two things instead of one
    /// — `-L` to discover, and the packaged `scanbus-scanimage` helper to acquire — and
    /// both are still read-only with respect to *this machine*. `scanimage` scans; it has
    /// no mode that installs software, and the helper is four lines of `exec`. The
    /// package query stayed `dpkg-query`, and what it is now asked about is one package
    /// from the distribution archive rather than two from a vendor download form, so the
    /// thing the user is told to run is `apt` and never this daemon.
    #[tokio::test]
    async fn the_install_path_only_ever_runs_read_only_queries() {
        let root = sysroot(&[]);
        let runner = Arc::new(RecordingRunner {
            registered: BTreeSet::from(["sane-airscan"]),
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
    ///
    /// **`&str` constants are excluded for the same reason, and 5.10 is why.** The one
    /// dependency left is in the distribution archive, so the message a user gets when it
    /// is missing is `apt install sane-airscan` — a sentence telling a human what to run,
    /// which is the *opposite* of this daemon running it. A guard that could not tell a
    /// message from an invocation would have forced that sentence to be spelled around,
    /// making the error worse to read in order to keep a grep happy. Nothing is lost:
    /// running a package manager needs a `Command::new`, and the count below is pinned at
    /// one — inside [`SystemCommandRunner`], whose whole argv the allowlist test asserts.
    #[test]
    fn the_backend_has_no_way_to_install_anything() {
        // Every non-test source file in the crate, not just this one: `skey` is where
        // the code that talks to a device now lives, and a guard that only covered
        // lib.rs would have stopped meaning anything the moment it was added.
        let sources = [
            ("lib.rs", include_str!("lib.rs")),
            ("acquisition.rs", include_str!("acquisition.rs")),
            ("listener.rs", include_str!("listener.rs")),
            ("registrar.rs", include_str!("registrar.rs")),
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
                .filter(|line| !line.trim_start().starts_with("const ") || !line.contains("&str"))
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
    ///
    /// [`crate::registrar`] is deliberately **not** on this list: it is the module that
    /// owns the socket and the clock, which is why the list is spelled out rather than
    /// being "every file under `src/`". Its own tests still need neither, because what it
    /// sends goes through a [`crate::registrar::SnmpTransport`] a test can supply.
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
