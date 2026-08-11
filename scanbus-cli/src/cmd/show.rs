//! `scanbus show <scanner>` — every `Scanner1` property, plus the scanner's buttons, in
//! one view ([`scanbus-cli.md`] §3).
//!
//! Resolution and reading happen against the same snapshot: `Objects::fetch` is what
//! finds the scanner *and* what supplies its buttons, so a scanner that gained a button
//! between the two calls this command would otherwise make cannot show one list that is
//! newer than the other.
//!
//! Buttons are listed by index and device label only — `Button1`'s own properties
//! (`Profile`, `ProfileOptions`) have no implementation on the daemon side yet ([2.5]),
//! and reading them here would be reaching past a contract that is not kept on the other
//! end. `scanbus button list` ([8.9]) is where the rest of a button's view belongs once
//! it exists.
//!
//! [2.5]: https://github.com/jeanparpaillon/scanbus_service/issues/9
//! [8.9]: https://github.com/jeanparpaillon/scanbus_service/issues/37
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

use scanbus_client::{Button, Objects, ScannerState};

use crate::cli::ScannerArg;
use crate::context::Context;
use crate::error::{Error, Result};
use crate::output::{self, Format};

use super::scanner_view;

/// Resolves `scanner`, reads its properties and buttons, and reports them.
///
/// # Errors
///
/// Exit 4 ([`Error`]) if `scanner` matches nothing or more than one object; otherwise
/// whatever the property read ends as.
pub async fn run(context: &Context, scanner: &ScannerArg) -> Result<u8> {
    let connection = context.connect().await?;
    let objects = context
        .within("listing the daemon's objects", Objects::fetch(&connection))
        .await?;

    let found = objects
        .scanner(&scanner.scanner, scanner.matching())
        .map_err(|error| Error::call("finding the scanner", error.into()))?;

    let state = context
        .within(
            format!("reading {}", found.id),
            ScannerState::fetch(&connection, &found.id),
        )
        .await?;

    let buttons: Vec<&Button> = objects
        .buttons()
        .iter()
        .filter(|button| button.scanner == found.id)
        .collect();

    report(context, &state, &buttons)?;
    Ok(0)
}

/// Writes the result in whichever format was asked for.
fn report(context: &Context, state: &ScannerState, buttons: &[&Button]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();

    match context.format {
        Format::Json => {
            let mut document = scanner_view::json(state);
            document["Buttons"] =
                serde_json::Value::Array(buttons.iter().map(|b| button_json(b)).collect());
            output::json(&mut stdout, &document)
        }
        Format::Human => human(&mut stdout, context, state, buttons),
    }
}

/// The property block, then a button table underneath — one view, per §3's checklist.
fn human(
    writer: &mut impl std::io::Write,
    context: &Context,
    state: &ScannerState,
    buttons: &[&Button],
) -> Result<()> {
    output::fields(
        writer,
        context.style,
        &[
            ("id", state.id.as_str().to_owned()),
            ("name", state.name.clone()),
            ("backend", state.backend.clone()),
            ("address", state.address.clone()),
            (
                "paired",
                (if state.paired { "yes" } else { "no" }).to_owned(),
            ),
            (
                "connected",
                (if state.connected { "yes" } else { "no" }).to_owned(),
            ),
            ("status", state.status.as_str().to_owned()),
            (
                "default profile",
                state
                    .default_profile
                    .map_or(String::new(), |profile| profile.as_str().to_owned()),
            ),
            (
                "supported profiles",
                state
                    .supported_profiles
                    .iter()
                    .map(|profile| profile.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            ("pairing state", state.pairing.as_str().to_owned()),
            ("pairing error", state.pairing.pairing_error().to_owned()),
        ],
    )?;

    if buttons.is_empty() {
        return Ok(());
    }

    writeln!(writer).map_err(Error::write)?;
    let rows: Vec<_> = buttons
        .iter()
        .map(|button| vec![button.index.to_string(), button.device_label.clone()])
        .collect();
    output::table(writer, context.style, &["IDX", "DEVICE LABEL"], &rows)
}

/// A button, named the way `Button1` would report it once it exists (§5): `Index` and
/// `DeviceLabel`, the two fields already reachable through `GetManagedObjects`.
fn button_json(button: &Button) -> serde_json::Value {
    serde_json::json!({
        "Index": button.index,
        "DeviceLabel": button.device_label,
    })
}
