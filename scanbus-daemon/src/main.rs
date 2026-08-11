//! scanbus daemon.
//!
//! Wiring only: it brings up the runtime and the log subscriber, then hands over to
//! [`scanbus_daemon`]. The D-Bus interfaces themselves (`Manager1`, `Scanner1`,
//! `Button1`, `Job1`) hang off the registry built here as the rest of workstream 2
//! lands.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use scanbus_daemon::dbus::{self, Manager1, ObjectRegistry, Profile1, path};
use scanbus_daemon::{
    Backends, Discovery, Error, JsonPairingStore, ProfileRegistry, ScannerRegistry,
};
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

/// How long the restore path (4.2) waits on a single scanner's `start_listening` before
/// moving on and publishing it offline.
///
/// Short enough that a switched-off scanner cannot make `org.scanbus` late: a device
/// that is actually there answers a local backend call well inside this, and one that
/// is not there is exactly the case the reconnect backoff exists for, not this timeout.
const RESTORE_TIMEOUT: Duration = Duration::from_secs(5);

/// Backends compiled into this binary, in the order they will be probed.
///
/// Empty on a default build: both backends are behind cargo features because they
/// shell out to hardware-specific tooling.
///
/// The order is the deduplication precedence — see [`scanbus_daemon::backends`] — which
/// is why the vendor backends come first: they are the ones that can deliver a button
/// press for the device they claim.
const BACKENDS: &[&str] = &[
    #[cfg(feature = "brother")]
    scanbus_backend_brother::ID,
    #[cfg(feature = "hplip")]
    scanbus_backend_hplip::ID,
    #[cfg(feature = "mobile")]
    scanbus_backend_mobile::ID,
];

/// The backend instances, in the same order as [`BACKENDS`].
///
/// Vendor backends are still skeletons. The mobile backend is available and discoverable
/// on default builds.
fn backends() -> Backends {
    let mut entries: Vec<Arc<dyn scanbus_core::ScannerBackend>> = Vec::new();

    #[cfg(feature = "mobile")]
    entries.push(Arc::new(scanbus_backend_mobile::MobileBackend::default()));

    Backends::new(entries)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    // RUST_LOG drives the filter; without it, info for our crates and warn elsewhere.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,scanbus_daemon=info,scanbus_core=info"));
    fmt().with_env_filter(filter).with_target(true).init();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        compiled_in = ?BACKENDS,
        probing = ?backends().ids(),
        "scanbus-daemon started"
    );

    match run().await {
        Ok(signal) => {
            info!(signal, "stopped");
            ExitCode::SUCCESS
        }
        Err(error) => {
            // Every variant here means the daemon is not serving: no bus, no name, or
            // an object tree it could not export. Exiting non-zero is what tells
            // systemd — and a developer starting a second instance by hand — that this
            // process is not the one answering on `org.scanbus`.
            error!(%error, "scanbus-daemon cannot run");
            ExitCode::FAILURE
        }
    }
}

/// Serves until a termination signal arrives, then takes the object tree down.
async fn run() -> Result<&'static str, Error> {
    let connection = dbus::connect().await?;

    // Objects first, name second: see the ordering in `dbus`. Everything later
    // workstreams restore or discover gets exported between these two lines.
    let registry = Arc::new(ObjectRegistry::new(connection.clone()).await?);
    let profiles = Arc::new(ProfileRegistry::new());
    let scanners = ScannerRegistry::new(
        Arc::clone(&registry),
        Arc::new(JsonPairingStore::new()),
        Arc::clone(&profiles),
    );
    let compiled_backends = backends();
    let discovery = Arc::new(Discovery::new(compiled_backends.clone(), Arc::clone(&scanners)));

    for kind in profiles.registered_profiles() {
        registry
            .add(path::profile(kind), Profile1::new(kind, Arc::clone(&profiles)))
            .await?;
    }

    registry
        .add(
            path::manager(),
            Manager1::new(Arc::clone(&discovery), Arc::clone(&profiles)),
        )
        .await?;

    // The last thing exported before the name: a client that resolves `org.scanbus` the
    // instant it appears must already find every paired scanner, listening if it was
    // connected before this restart (4.2).
    scanners.restore(&compiled_backends, RESTORE_TIMEOUT).await;

    dbus::request_name(&connection).await?;

    let signal = shutdown_signal().await.map_err(Error::Signal)?;
    info!(signal, "shutting down");

    // Before the tree goes: the session task publishes objects, and one still running
    // during the unexports would be racing them.
    discovery.stop().await;
    // Likewise for the listener tasks, which look their object up on every press: one
    // still running during the unexports would spend the daemon's last seconds warning
    // about objects that are on their way out.
    scanners.shutdown().await;

    // Explicitly, rather than leaving it to `Drop`: this is the one place that can
    // await the unexports, so clients see `InterfacesRemoved` for every object instead
    // of just watching the name vanish.
    registry.shutdown().await;

    // Released explicitly rather than left to the connection's `Drop`, so a restart
    // that races its own predecessor never finds the name still owned by a process on
    // its way out.
    if let Err(error) = connection.release_name(dbus::BUS_NAME).await {
        warn!(%error, "could not release the bus name");
    }

    Ok(signal)
}

/// Resolves with the name of the first termination signal received.
///
/// systemd sends SIGTERM on `stop` and on `restart`; SIGINT is what a developer
/// running the binary in a terminal sends.
async fn shutdown_signal() -> std::io::Result<&'static str> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    Ok(tokio::select! {
        _ = sigterm.recv() => "SIGTERM",
        _ = sigint.recv() => "SIGINT",
    })
}
