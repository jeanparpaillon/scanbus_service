//! scanbus daemon.
//!
//! Skeleton: it brings up the runtime, the log subscriber and the shutdown path, and
//! nothing else. The D-Bus interfaces (`Manager1`, `Scanner1`, `Button1`, `Job1`) land
//! in workstream 2 and hang off this `main`.

use std::process::ExitCode;

use tracing::info;
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

    match shutdown_signal().await {
        Ok(signal) => {
            info!(signal, "shutting down");
            ExitCode::SUCCESS
        }
        Err(error) => {
            // Losing the signal handlers would mean systemd's SIGTERM goes to the
            // default disposition and the daemon dies without running its shutdown
            // path. Refuse to run half-supervised.
            tracing::error!(%error, "cannot install signal handlers");
            ExitCode::FAILURE
        }
    }
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
