//! `scanbus pair`, `cancel-pairing` and `unpair` ([`scanbus-cli.md`] §3, §7, issue [8.6]).
//!
//! `pair` is where the two race conditions of §7 both matter at once:
//!
//! **Subscribe before you call.** [`ScannerWatch::subscribe`] installs the match rule
//! before `Pair()` is called, so a scanner whose backend is already installed cannot
//! reach `done` before anything is listening for it — see that type's own module
//! documentation for why the snapshot afterwards is still the right first event.
//!
//! **A scanner that only exists because of a discovery session.** An unpaired target's
//! object disappears when whatever session found it ends (API §1) — including a session
//! this process did not start. `pair` therefore holds its own [`Discovery`] whenever the
//! target is unpaired or not yet resolved, for the whole of the pairing, and releases it
//! once the verdict is in. When the selector matches nothing at all, the same session is
//! what a short discovery runs under before giving up.
//!
//! [8.6]: https://github.com/jeanparpaillon/scanbus_service/issues/33
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

use std::collections::HashMap;
use std::io::{IsTerminal as _, Write as _};

use futures_util::{FutureExt as _, StreamExt as _};
use scanbus_client::proxy::{Manager1Proxy, Scanner1Proxy};
use scanbus_client::{
    Connection, Error as ClientError, Objects, ScanbusError, Scanner, ScannerAdditions,
    ScannerState, ScannerWatch, SelectError,
};

use crate::cli::ScannerArg;
use crate::context::Context;
use crate::error::{Error, Result};
use crate::output::{self, Format};

use super::scanner_view;

/// `scanbus pair <scanner> [--no-wait]`.
///
/// # Errors
///
/// Exit 4 if the selector never resolves, exit 6 if it is already paired, exit 9 if the
/// pairing ends in `PairingState="failed"`, exit 12 if `--timeout` elapses first —
/// [`scanbus-cli.md`] §8.
pub async fn run(context: &Context, scanner: &ScannerArg, no_wait: bool) -> Result<u8> {
    let connection = context.connect().await?;
    let mut discovery = Discovery::default();

    let found = match try_match(context, &connection, scanner).await? {
        Some(found) => found,
        None => match discover(context, &connection, scanner, &mut discovery).await? {
            Outcome::Found(found) => found,
            Outcome::TimedOut => {
                discovery.stop(context, &connection).await?;
                return Err(unresolved(context, &connection, scanner).await?);
            }
            Outcome::Interrupted => {
                discovery.stop(context, &connection).await?;
                return Ok(130);
            }
        },
    };

    let state = context
        .within(format!("reading {}", found.id), ScannerState::fetch(&connection, &found.id))
        .await?;

    if state.paired {
        discovery.stop(context, &connection).await?;
        return Err(already_paired(&found));
    }

    // Unpaired: the object exists only for as long as some discovery session does
    // (§7), and that session may not be ours — holding one of our own for the
    // duration is what stops it disappearing mid-install.
    discovery.start(context, &connection).await?;

    let result = pair_and_wait(context, &connection, &found, no_wait).await;
    discovery.stop(context, &connection).await?;
    result
}

/// `scanbus cancel-pairing <scanner>`.
///
/// # Errors
///
/// Exit 4 if the selector does not resolve; tolerates a pairing that has already gone
/// terminal, per `CancelPairing()`'s own contract.
pub async fn cancel(context: &Context, scanner: &ScannerArg) -> Result<u8> {
    let connection = context.connect().await?;
    let found = resolve(context, &connection, scanner).await?;

    let proxy = context
        .within(format!("resolving {}", found.id), Scanner1Proxy::for_scanner(&connection, &found.id))
        .await?;

    context
        .within(format!("cancelling the pairing of {}", found.id), async {
            proxy.cancel_pairing().await.map_err(ClientError::from)
        })
        .await?;

    Ok(0)
}

/// `scanbus unpair <scanner> [--yes]`.
///
/// # Errors
///
/// Exit 4 if the selector does not resolve, exit 7 on `org.scanbus.Error.NotPaired`.
/// Without `--yes`, refuses rather than prompting when stdin is not a terminal.
pub async fn unpair(context: &Context, scanner: &ScannerArg, yes: bool) -> Result<u8> {
    let connection = context.connect().await?;
    let found = resolve(context, &connection, scanner).await?;

    if !yes && !confirm(&found)? {
        return Ok(1);
    }

    let proxy = context
        .within(format!("resolving {}", found.id), Scanner1Proxy::for_scanner(&connection, &found.id))
        .await?;

    context
        .within(format!("unpairing {}", found.id), async {
            proxy.unpair().await.map_err(ClientError::from)
        })
        .await?;

    Ok(0)
}

/// Calls `Pair()`, then follows `PairingState` to a verdict — or returns immediately
/// under `--no-wait`, which is what the D-Bus call itself does.
async fn pair_and_wait(
    context: &Context,
    connection: &Connection,
    found: &Scanner,
    no_wait: bool,
) -> Result<u8> {
    // Step 1: subscribed before step 2 makes the call — scanbus-cli.md §7.
    let watch = context
        .within(
            format!("subscribing to {}", found.id),
            ScannerWatch::subscribe(connection, found.path()),
        )
        .await?;

    let proxy = context
        .within(format!("resolving {}", found.id), Scanner1Proxy::for_scanner(connection, &found.id))
        .await?;

    context
        .within(format!("pairing {}", found.id), async {
            proxy.pair(HashMap::new()).await.map_err(ClientError::from)
        })
        .await?;

    // Steps 3 and 4: the snapshot, then the changes strictly after it.
    let mut states = context
        .within(format!("reading {}", found.id), watch.states())
        .await?;

    let Some(first) = states.next().await else {
        return Err(stream_ended(found));
    };
    let first = first.map_err(|error| Error::call(format!("reading {}", found.id), error))?;
    print_transition(context, &first)?;

    if let Some(code) = terminal(context, &first)? {
        return Ok(code);
    }

    if no_wait {
        return Ok(0);
    }

    let deadline = tokio::time::sleep(context.timeout);
    tokio::pin!(deadline);
    let sigint = tokio::signal::ctrl_c();
    tokio::pin!(sigint);

    loop {
        tokio::select! {
            state = states.next() => {
                let Some(state) = state else {
                    return Err(stream_ended(found));
                };
                let state = state.map_err(|error| Error::call(format!("reading {}", found.id), error))?;
                print_transition(context, &state)?;

                if let Some(code) = terminal(context, &state)? {
                    return Ok(code);
                }
            }
            () = &mut deadline => return Err(Error::wait_timeout(format!("waiting for {} to pair", found.id), context.timeout)),
            _ = &mut sigint => {
                eprintln!(
                    "scanbus: interrupted — pairing continues on the daemon; \
                     run `scanbus cancel-pairing {}` to stop it",
                    found.id
                );
                return Ok(130);
            }
        }
    }
}

/// Whether `state` is a terminal `PairingState`, and what the process should end as if
/// so — printing the final document under `--json`, since `pair` is single-shot there.
fn terminal(context: &Context, state: &ScannerState) -> Result<Option<u8>> {
    if state.pairing.as_str() == "done" {
        report_final(context, state)?;
        return Ok(Some(0));
    }

    if state.pairing.is_failed() {
        return Err(Error::call(
            "pairing",
            ClientError::Call(ScanbusError::BackendInstallFailed(
                state.pairing.pairing_error().to_owned(),
            )),
        ));
    }

    Ok(None)
}

/// One `PairingState` transition, printed as it arrives — human output only; `--json`
/// prints the single final document instead (§6).
fn print_transition(context: &Context, state: &ScannerState) -> Result<()> {
    if context.format != Format::Human {
        return Ok(());
    }

    let mut stdout = std::io::stdout().lock();
    let line = if state.pairing.is_failed() {
        format!("{:<12}{}", state.pairing.as_str(), state.pairing.pairing_error())
    } else {
        format!("{:<12}{}", state.pairing.as_str(), state.id)
    };
    writeln!(stdout, "{line}").map_err(Error::write)
}

/// The single JSON document `--json` prints once pairing reaches `done`.
fn report_final(context: &Context, state: &ScannerState) -> Result<()> {
    if context.format != Format::Json {
        return Ok(());
    }

    let mut stdout = std::io::stdout().lock();
    output::json(&mut stdout, &scanner_view::json(state))
}

/// What ended a search: the value it was looking for, or why it gave up without one.
enum Outcome<T> {
    Found(T),
    TimedOut,
    Interrupted,
}

/// Holds — or does not — a discovery session this process started.
///
/// The same best-effort ownership guess `discover` makes (§7): calling `StartDiscovery`
/// when a session is already running is harmless (it answers as soon as the session is
/// up, per the API), and `StopDiscovery` afterwards is this process cleaning up
/// specifically the session *it* asked for, not a claim about anyone else's.
#[derive(Default)]
struct Discovery {
    active: bool,
}

impl Discovery {
    async fn start(&mut self, context: &Context, connection: &Connection) -> Result<()> {
        if self.active {
            return Ok(());
        }

        context
            .within("starting discovery", async {
                let manager = Manager1Proxy::new(connection).await?;
                manager.start_discovery(HashMap::new()).await?;
                Ok::<_, ClientError>(())
            })
            .await?;

        self.active = true;
        Ok(())
    }

    async fn stop(&mut self, context: &Context, connection: &Connection) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;

        context
            .within("stopping discovery", async {
                let manager = Manager1Proxy::new(connection).await?;
                manager.stop_discovery().await?;
                Ok::<_, ClientError>(())
            })
            .await
    }
}

/// Holds `discovery`, subscribing before `StartDiscovery` (§7), and waits for `scanner`
/// to appear.
async fn discover(
    context: &Context,
    connection: &Connection,
    scanner: &ScannerArg,
    discovery: &mut Discovery,
) -> Result<Outcome<Scanner>> {
    let added_stream = context
        .within(
            "subscribing to newly discovered scanners",
            ScannerAdditions::subscribe(connection),
        )
        .await?;
    let mut added = std::pin::pin!(added_stream);

    discovery.start(context, connection).await?;

    // Whatever the daemon had already added by the time `StartDiscovery` returned is
    // reflected in the snapshot `try_match` reads next — see `PropertyWatch`'s module
    // docs for why draining what is already buffered here is sound.
    while added.next().now_or_never().flatten().is_some() {}

    if let Some(found) = try_match(context, connection, scanner).await? {
        return Ok(Outcome::Found(found));
    }

    let deadline = tokio::time::sleep(context.timeout);
    tokio::pin!(deadline);
    let sigint = tokio::signal::ctrl_c();
    tokio::pin!(sigint);

    loop {
        tokio::select! {
            item = added.next() => {
                if item.is_none() {
                    return Ok(Outcome::TimedOut);
                }
                if let Some(found) = try_match(context, connection, scanner).await? {
                    return Ok(Outcome::Found(found));
                }
            }
            () = &mut deadline => return Ok(Outcome::TimedOut),
            _ = &mut sigint => return Ok(Outcome::Interrupted),
        }
    }
}

/// One attempt at resolving `scanner`, treating "matched nothing" as absence rather
/// than failure — the caller decides what that means.
///
/// # Errors
///
/// [`Error`] with exit 4 for an ambiguous selector; ambiguity is never resolved by
/// waiting for more scanners to appear.
async fn try_match(
    context: &Context,
    connection: &Connection,
    scanner: &ScannerArg,
) -> Result<Option<Scanner>> {
    let objects = context
        .within("listing the daemon's objects", Objects::fetch(connection))
        .await?;

    match objects.scanner(&scanner.scanner, scanner.matching()) {
        Ok(found) => Ok(Some(found.clone())),
        Err(SelectError::NotFound { .. }) => Ok(None),
        Err(error @ SelectError::Ambiguous { .. }) => {
            Err(Error::call("finding the scanner", error.into()))
        }
    }
}

/// A selector that resolves, or fails with exit 4 and the candidates §8 promises.
async fn resolve(context: &Context, connection: &Connection, scanner: &ScannerArg) -> Result<Scanner> {
    let objects = context
        .within("listing the daemon's objects", Objects::fetch(connection))
        .await?;

    objects
        .scanner(&scanner.scanner, scanner.matching())
        .cloned()
        .map_err(|error| Error::call("finding the scanner", error.into()))
}

/// The final report once a short discovery gave up without a match: one more read of
/// the object tree, so the message carries whatever the daemon does know about.
async fn unresolved(context: &Context, connection: &Connection, scanner: &ScannerArg) -> Result<Error> {
    let objects = context
        .within("listing the daemon's objects", Objects::fetch(connection))
        .await?;

    let error = objects
        .scanner(&scanner.scanner, scanner.matching())
        .expect_err("a scanner discovery just gave up on would not now resolve");

    Ok(Error::call("finding the scanner", error.into()))
}

/// `org.scanbus.Error.AlreadyPaired`, with the hint §8's checklist asks for.
fn already_paired(found: &Scanner) -> Error {
    Error::call(
        format!("pairing {}", found.id),
        ClientError::Call(ScanbusError::AlreadyPaired(format!(
            "{} is already paired — see `scanbus unpair {}`",
            found.id, found.id
        ))),
    )
}

/// The `PairingState` stream ended without reaching a terminal state — not documented
/// behaviour of this daemon, but not a `zbus` transport failure either.
fn stream_ended(found: &Scanner) -> Error {
    Error::call(
        format!("waiting for {} to pair", found.id),
        ClientError::Call(ScanbusError::Other {
            name: "org.scanbus.internal.StreamEnded".to_owned(),
            message: "PropertiesChanged stopped before PairingState reached done or failed"
                .to_owned(),
        }),
    )
}

/// Prompts on a TTY, refuses instead of blocking on anything else.
fn confirm(found: &Scanner) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Err(Error::call(
            format!("unpairing {}", found.id),
            ClientError::Call(ScanbusError::Other {
                name: "org.scanbus.internal.NoTTY".to_owned(),
                message: "refusing to prompt on a non-terminal stdin; pass --yes".to_owned(),
            }),
        ));
    }

    eprint!("Unpair {} ({})? [y/N] ", found.id, found.name);
    std::io::stderr().flush().map_err(Error::write)?;

    let mut line = String::new();
    std::io::stdin().read_line(&mut line).map_err(Error::write)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}
