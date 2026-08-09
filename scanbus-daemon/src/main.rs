//! scanbus daemon.
//!
//! Wiring only: it brings up the runtime and the log subscriber, then hands over to
//! [`scanbus_daemon`]. The D-Bus interfaces themselves (`Manager1`, `Scanner1`,
//! `Button1`, `Job1`) hang off the registry built here as the rest of workstream 2
//! lands.

use std::process::ExitCode;

use scanbus_daemon::Error;
use scanbus_daemon::dbus::{self, ObjectRegistry};
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, fmt};

/// Backends compiled into this binary, in the order they will be probed.
///
/// Empty on a default build: both backends are behind cargo features because they
/// shell out to hardware-specific tooling.
const BACKENDS: &[&str] = &[
    #[cfg(feature = "brother")]
    scanbus_backend_brother::ID,
    #[cfg(feature = "hplip")]
    scanbus_backend_hplip::ID,
];

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    // RUST_LOG drives the filter; without it, info for our crates and warn elsewhere.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,scanbus_daemon=info,scanbus_core=info"));
    fmt().with_env_filter(filter).with_target(true).init();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        backends = ?BACKENDS,
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
    let registry = ObjectRegistry::new(connection.clone()).await?;
    dbus::request_name(&connection).await?;

    let signal = shutdown_signal().await.map_err(Error::Signal)?;
    info!(signal, "shutting down");

    // Explicitly, rather than leaving it to `Drop`: this is the one place that can
    // await the unexports, so clients see `InterfacesRemoved` for every object instead
    // of just watching the name vanish.
    registry.shutdown().await;

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
