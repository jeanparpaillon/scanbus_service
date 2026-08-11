//! `scanbus discover` — starts a discovery session and streams what appears
//! ([`scanbus-cli.md`] §3, §4, §7).
//!
//! Two things make this different from every other command:
//!
//! **It streams rather than batching.** `InterfacesAdded` exists so that a scanner shows
//! up when it shows up (API §2); collecting for `--for`'s duration and printing a table
//! at the end would throw away the only property that makes the signal worth having.
//! The pattern is §7's raceless one, applied to the object tree instead of one object's
//! properties: subscribe to `InterfacesAdded` *before* calling `StartDiscovery`, then
//! read `GetManagedObjects` once the call has returned, and treat everything already in
//! that snapshot as the first batch rather than risking it arriving twice — once from
//! the snapshot, once from a signal that was already buffered when the snapshot was
//! taken. [`crate::watch::PropertyWatch`] documents why that draining is sound; the
//! reasoning is identical here, one level up.
//!
//! **It does not need to know whether it owns the session, any more.** The guess below
//! predates [2.9]: with no owner recorded anywhere, calling `StopDiscovery`
//! unconditionally on exit could take down a GUI client's session a moment after it
//! started one, so this command inferred ownership instead — if no *unpaired* scanner
//! existed before it called `StartDiscovery`, nobody else's session could have been
//! running, and it stops the one it started; if one did, it leaves it alone and says so
//! rather than silently keeping it running. `--keep` is the explicit version of the same
//! escape hatch. The daemon now reference-counts the session by caller bus name, so
//! `StopDiscovery` releases only this process's share and neither the guess nor `--keep`
//! protects anything — and neither can extend a session past this process's exit, which
//! drops its reference. Both are kept until the command's own issue revisits its surface:
//! removing a documented flag is not this daemon change's to make.
//!
//! [2.9]: https://github.com/jeanparpaillon/scanbus_service/issues/34
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

use std::collections::HashSet;
use std::time::Duration;

use futures_util::{FutureExt as _, Stream, StreamExt as _};
use scanbus_client::proxy::{self, Manager1Proxy};
use scanbus_client::{Connection, Error as ClientError, Objects, ScannerAdditions, ScannerState};

use crate::context::Context;
use crate::error::{Error, Result};
use crate::output::{self, Format};

use super::scanner_view;

/// Runs the session for `duration` (or until `SIGINT` under `--watch`), streaming every
/// scanner as it appears.
///
/// # Errors
///
/// [`Error`] if the connection, the subscription or `StartDiscovery` fails —
/// `org.freedesktop.DBus.Error.InvalidArgs` among them, for a `--backend` name the
/// daemon does not recognise.
pub async fn run(
    context: &Context,
    backends: &[String],
    duration: Duration,
    watch: bool,
    keep: bool,
) -> Result<u8> {
    let connection = context.connect().await?;

    // Step 1: subscribe before the call that changes anything — see the module header.
    let added_stream = context
        .within(
            "subscribing to newly discovered scanners",
            ScannerAdditions::subscribe(&connection),
        )
        .await?;
    let mut added = std::pin::pin!(added_stream);

    // Whether an unpaired scanner already existed: if one did, some other session is
    // already running and this process must not guess that it owns it.
    let owns_session = no_unpaired_scanner_exists(context, &connection).await?;

    start_discovery(context, &connection, backends).await?;

    // Step 3, and the dedup that comes with it: whatever the daemon had already added by
    // the time this reply landed is reflected in it, per the same ordering guarantee
    // `PropertyWatch::snapshot` relies on — so anything already buffered on `added` at
    // this point is a duplicate of something this snapshot already holds.
    while added.next().now_or_never().flatten().is_some() {}

    let snapshot = context
        .within("listing the daemon's objects", Objects::fetch(&connection))
        .await?;

    let mut seen: HashSet<String> = HashSet::new();
    let mut states = Vec::new();
    for scanner in snapshot.scanners() {
        if let Some(state) = scanner_view::fetch(context, &connection, scanner).await? {
            seen.insert(state.id.as_str().to_owned());
            states.push(state);
        }
    }

    let mut stdout = std::io::stdout().lock();
    print_header(&mut stdout, context)?;
    for state in &states {
        print_scanner(&mut stdout, context, state)?;
    }

    let interrupted = stream(context, &mut stdout, &mut added, &mut seen, duration, watch).await?;
    drop(stdout);

    stop(context, &connection, owns_session, keep).await?;

    Ok(if interrupted { 130 } else { 0 })
}

/// Whether nothing unpaired is visible yet — the ownership guess documented in this
/// module's header, which [2.9](https://github.com/jeanparpaillon/scanbus_service/issues/34)
/// has since made unnecessary rather than wrong.
async fn no_unpaired_scanner_exists(context: &Context, connection: &Connection) -> Result<bool> {
    let objects = context
        .within("listing the daemon's objects", Objects::fetch(connection))
        .await?;

    for scanner in objects.scanners() {
        let Some(state) = scanner_view::fetch(context, connection, scanner).await? else {
            continue;
        };
        if !state.paired {
            return Ok(false);
        }
    }

    Ok(true)
}

/// `StartDiscovery`, with `--backend` rendered as the `filters` map §2 defines.
async fn start_discovery(
    context: &Context,
    connection: &Connection,
    backends: &[String],
) -> Result<()> {
    let filters = proxy::backend_filters(backends);

    context
        .within("starting discovery", async {
            let manager = Manager1Proxy::new(connection).await?;
            manager.start_discovery(filters).await?;
            Ok::<_, ClientError>(())
        })
        .await
}

/// Streams arrivals until `duration` elapses (or forever, under `--watch`), or `SIGINT`.
///
/// Returns whether it ended on `SIGINT`.
async fn stream(
    context: &Context,
    writer: &mut impl std::io::Write,
    added: &mut (impl Stream<Item = scanbus_client::Result<ScannerState>> + Unpin),
    seen: &mut HashSet<String>,
    duration: Duration,
    watch: bool,
) -> Result<bool> {
    let deadline = async {
        if watch {
            std::future::pending::<()>().await;
        } else {
            tokio::time::sleep(duration).await;
        }
    };
    tokio::pin!(deadline);

    let sigint = tokio::signal::ctrl_c();
    tokio::pin!(sigint);

    loop {
        tokio::select! {
            signal = added.next() => {
                let Some(signal) = signal else { break; };
                let Ok(state) = signal else { continue; };

                if seen.insert(state.id.as_str().to_owned()) {
                    print_scanner(writer, context, &state)?;
                }
            }
            () = &mut deadline => break,
            _ = &mut sigint => return Ok(true),
        }
    }

    Ok(false)
}

/// Releases the session unless `--keep` was given or this process does not believe it
/// started it — see the module header.
async fn stop(context: &Context, connection: &Connection, owns_session: bool, keep: bool) -> Result<()> {
    if keep {
        return Ok(());
    }

    if !owns_session {
        if context.format == Format::Human {
            eprintln!(
                "scanbus: leaving discovery running — another client appears to already be \
                 using it (best-effort guess; pass --keep to silence this)"
            );
        }
        return Ok(());
    }

    context
        .within("stopping discovery", async {
            let manager = Manager1Proxy::new(connection).await?;
            manager.stop_discovery().await?;
            Ok::<_, ClientError>(())
        })
        .await?;

    if context.format == Format::Human {
        eprintln!(
            "scanbus: discovery stopped — the unpaired scanners just shown no longer exist; \
             `scanbus pair` re-discovers them as needed"
        );
    }

    Ok(())
}

/// The table header, printed once in human mode; nothing in `--json`, where each row is
/// its own document.
fn print_header(writer: &mut impl std::io::Write, context: &Context) -> Result<()> {
    if context.format != Format::Human {
        return Ok(());
    }

    writeln!(
        writer,
        "{}",
        context.style.bold(&scanner_view::HEADERS.join("  "))
    )
    .map_err(Error::write)
}

/// One scanner, as a table row or a JSON Lines `"added"` event (§6).
fn print_scanner(
    writer: &mut impl std::io::Write,
    context: &Context,
    state: &ScannerState,
) -> Result<()> {
    match context.format {
        Format::Json => {
            let event = serde_json::json!({
                "event": "added",
                "path": state.path(),
                "interfaces": { "org.scanbus.Scanner1": scanner_view::json(state) },
            });
            output::json(writer, &event)
        }
        Format::Human => {
            writeln!(writer, "{}", scanner_view::row(state).join("  ")).map_err(Error::write)
        }
    }
}
