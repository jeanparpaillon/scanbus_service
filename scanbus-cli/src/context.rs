//! The global options, resolved once, and the two things every command does with them.
//!
//! A command is handed a [`Context`] rather than the parsed flags: `--bus`,
//! `--no-activate` and `--timeout` are answers to questions a subcommand should not be
//! asking again, and a second command that decided for itself what `--timeout` bounds
//! would be the start of the drift §3's table exists to prevent.
//!
//! [`Context::within`] is where `--timeout` becomes real. zbus has no per-call deadline
//! of its own — a method call waits for a reply until the bus gives up, which for a
//! daemon that is alive but wedged is never — so the bound is applied here, around every
//! call, by the one place that knows what the user asked for.

use std::time::Duration;

// `Connection` comes through the client crate rather than from zbus: §2 lists this
// crate's dependencies as scanbus-client, clap, tokio and serde_json, and a `zbus` in
// this manifest would be the first step towards a proxy declared here.
use scanbus_client::{Bus, Connection, Error as ClientError};

use crate::cli::Global;
use crate::error::{Error, Result};
use crate::output::{Format, Style};

/// The global options of §3, resolved.
#[derive(Debug)]
pub struct Context {
    /// Which bus to talk to.
    pub bus: Bus,
    /// Human or JSON.
    pub format: Format,
    /// Whether output may carry escapes.
    pub style: Style,
    /// The ceiling on one D-Bus call.
    pub timeout: Duration,
    /// Whether a call may start the daemon.
    pub activate: bool,
}

impl Context {
    /// Resolves the global options, consulting the environment for colour.
    pub fn new(global: &Global) -> Self {
        let format = Format::new(global.json);

        Self {
            bus: global.bus.clone(),
            format,
            style: Style::detect(global.no_color, format),
            timeout: global.timeout,
            activate: !global.no_activate,
        }
    }

    /// Opens the connection every command but `status` needs.
    ///
    /// With `--no-activate` this costs one round trip to the bus daemon and refuses
    /// before any call is made, which is the only implementation of the flag that does
    /// not do the thing it was asked not to do — see [`scanbus_client::connect`].
    ///
    /// # Errors
    ///
    /// [`Error`] with exit 3 when the daemon is not running and may not be started, and
    /// exit 1 when the bus itself cannot be reached.
    pub async fn connect(&self) -> Result<Connection> {
        let what = format!("connecting to the {} bus", self.bus);
        self.within(what, scanbus_client::connect(&self.bus, self.activate))
            .await
    }

    /// Runs one D-Bus call under `--timeout`, labelling whatever it ends as.
    ///
    /// `what` is the gerund phrase §6 puts after the program name: `"reading the profile
    /// types"` becomes `scanbus: reading the profile types: …`.
    ///
    /// # Errors
    ///
    /// Whatever the call ends as, or [`Error::timeout`] when `--timeout` elapses first.
    pub async fn within<T>(
        &self,
        what: impl Into<String>,
        call: impl Future<Output = std::result::Result<T, ClientError>>,
    ) -> Result<T> {
        let what = what.into();

        match tokio::time::timeout(self.timeout, call).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(Error::call(what, error)),
            Err(_elapsed) => Err(Error::timeout(what, self.timeout)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    use crate::cli::Cli;

    fn context(args: &[&str]) -> Context {
        Context::new(&Cli::try_parse_from(args).unwrap().global)
    }

    /// `--no-activate` is stored as its opposite, because every use of it reads better
    /// that way and one negation is easier to get right than several.
    #[test]
    fn no_activate_becomes_activate() {
        assert!(context(&["scanbus", "status"]).activate);
        assert!(!context(&["scanbus", "--no-activate", "status"]).activate);
    }

    #[test]
    fn the_global_options_land_where_the_commands_look_for_them() {
        let context = context(&[
            "scanbus",
            "--json",
            "--timeout",
            "2s",
            "--bus",
            "unix:path=/tmp/x",
            "status",
        ]);

        assert_eq!(context.format, Format::Json);
        assert_eq!(context.timeout, Duration::from_secs(2));
        assert_eq!(context.bus, Bus::Address("unix:path=/tmp/x".to_owned()));
        assert!(!context.style.enabled(), "--json implies --no-color");
    }

    /// The bound is on the call, not on the process: a call that never answers must end
    /// as a timeout with the phrase that says what was being attempted.
    #[tokio::test]
    async fn a_call_that_never_answers_ends_as_a_timeout() {
        let context = context(&["scanbus", "--timeout", "10ms", "status"]);

        let error = context
            .within("reading the profile types", async {
                std::future::pending::<std::result::Result<(), ClientError>>().await
            })
            .await
            .expect_err("a pending call must not outlive --timeout");

        assert_eq!(error.exit_code(), 1);
        assert_eq!(
            error.to_string(),
            "reading the profile types: no reply after 10ms"
        );
    }

    /// A call that answers in time is untouched, timeout or no timeout.
    #[tokio::test]
    async fn a_call_that_answers_passes_through() {
        let context = context(&["scanbus", "status"]);
        let value = context
            .within("reading the profile types", async { Ok(42) })
            .await
            .unwrap();

        assert_eq!(value, 42);
    }
}
