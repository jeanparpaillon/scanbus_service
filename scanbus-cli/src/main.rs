//! `scanbus` — the command-line client for the scanbus D-Bus service.
//!
//! Wiring only, and three decisions that belong to a process rather than to a command:
//!
//! - **a current-thread runtime**. This process makes a handful of D-Bus calls and
//!   exits; a thread pool would be startup cost with nothing to run on it, and the
//!   streaming commands ([8.5], [8.8]) are waiting on a socket, not on a CPU.
//! - **logging to stderr, off by default**. `-v` is the client's own `tracing`, and
//!   nothing else: the daemon's log level is the daemon's, and a `-vv` that silently
//!   reconfigured a system service would be a surprising thing for a `--verbose` flag to
//!   do. `journalctl --user -u scanbus` is what the help text points at instead.
//! - **the exit code is the interface**. [`scanbus-cli.md`] §8 gives one code per named
//!   error so a script can branch on *why* without parsing English, which means the only
//!   `main` that can hold that contract is one whose every path ends in an
//!   [`Error::exit_code`](error::Error::exit_code) or a code a command chose.
//!
//! [8.5]: https://github.com/jeanparpaillon/scanbus_service/issues/32
//! [8.8]: https://github.com/jeanparpaillon/scanbus_service/issues/36
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

mod cli;
mod cmd;
mod context;
mod duration;
mod error;
mod output;

use std::process::ExitCode;

use clap::Parser as _;
use tracing_subscriber::{EnvFilter, fmt};

use crate::cli::Cli;
use crate::context::Context;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // `parse` exits 2 on a usage error and 0 on `--help`, both before anything below
    // runs — which is why the runtime being already started costs nothing.
    let cli = Cli::parse();

    init_tracing(cli.global.verbose);
    let context = Context::new(&cli.global);

    match cmd::dispatch(&context, &cli.command).await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            // §6: `scanbus: <what failed>: <D-Bus error name>: <message>`, on stderr,
            // including under `--json` — stdout stays parseable.
            eprintln!("scanbus: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}

/// Client-side logging to stderr: silent, `info`, then `debug`.
///
/// `RUST_LOG` still wins where it is set, because a developer chasing zbus itself needs
/// a filter this flag cannot express (`-vv` is our crates, not the whole tree). Without
/// it, `-v` says nothing about zbus's internals, which at `debug` are a wall of message
/// traffic that buries the client's own lines.
fn init_tracing(verbosity: u8) {
    let level = match verbosity {
        0 => return,
        1 => "info",
        _ => "debug",
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(format!("warn,scanbus_cli={level},scanbus_client={level}"))
    });

    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(verbosity > 1)
        .init();
}
