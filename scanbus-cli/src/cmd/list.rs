//! `scanbus list` — [`scanbus-cli.md`] §3: scanners the daemon knows about *right now*.
//!
//! No discovery, ever: a freshly restarted daemon with two restored pairings lists two
//! scanners and probes nothing, because [`scanbus_client::Objects::fetch`] is one
//! `GetManagedObjects` and nothing else. `scanbus discover` is the command that goes
//! looking ([8.5]).
//!
//! [8.5]: https://github.com/jeanparpaillon/scanbus_service/issues/32
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

use scanbus_client::{Objects, ScannerState};

use crate::context::Context;
use crate::error::Result;
use crate::output::{self, Format};

use super::scanner_view;

/// Lists what `GetManagedObjects` holds, filtered by `--paired`/`--unpaired`.
///
/// # Errors
///
/// [`Error`](crate::error::Error) for anything a call to the daemon ends as. A scanner
/// that vanished between the snapshot and its own property read is silently dropped —
/// see [`scanner_view::fetch`] — rather than failing the whole listing.
pub async fn run(context: &Context, paired: bool, unpaired: bool) -> Result<u8> {
    let connection = context.connect().await?;
    let objects = context
        .within("listing the daemon's objects", Objects::fetch(&connection))
        .await?;

    let mut states: Vec<ScannerState> = Vec::new();
    for scanner in objects.scanners() {
        if let Some(state) = scanner_view::fetch(context, &connection, scanner).await? {
            states.push(state);
        }
    }
    states.retain(|state| (!paired || state.paired) && (!unpaired || !state.paired));

    report(context, &states)?;
    Ok(0)
}

/// Writes the result in whichever format was asked for.
fn report(context: &Context, states: &[ScannerState]) -> Result<()> {
    match context.format {
        Format::Json => {
            let mut stdout = std::io::stdout().lock();
            let array: Vec<_> = states.iter().map(scanner_view::json).collect();
            output::json(&mut stdout, &serde_json::Value::Array(array))?;
        }
        Format::Human => {
            if states.is_empty() {
                eprintln!("scanbus: the daemon has no scanners to list right now");
            } else {
                let mut stdout = std::io::stdout().lock();
                let rows: Vec<_> = states.iter().map(scanner_view::row).collect();
                output::table(&mut stdout, context.style, &scanner_view::HEADERS, &rows)?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use scanbus_core::{PairingState, ScannerId, Status};

    use super::*;

    fn scanner(id: &str, paired: bool) -> ScannerState {
        ScannerState {
            id: ScannerId::new(id).unwrap(),
            name: String::new(),
            backend: "mock".to_owned(),
            address: String::new(),
            capabilities: scanbus_core::Capabilities::default(),
            supported_profiles: Vec::new(),
            paired,
            connected: false,
            status: Status::Online,
            default_profile: None,
            pairing: PairingState::None,
        }
    }

    /// A row's cells are §4's five columns, in order.
    #[test]
    fn a_row_is_the_five_documented_columns() {
        let row = scanner_view::row(&scanner("brother_net_192_2E168_2E1_2E23", true));
        assert_eq!(
            row,
            [
                "mock",
                "brother_net_192_2E168_2E1_2E23",
                "",
                "online",
                "yes"
            ]
        );
    }

    /// `Paired` in `--json` is a boolean, not the table's `yes`/`no` — a `jq
    /// '.[].Paired'` reads the same type `Properties.GetAll` would have sent.
    #[test]
    fn the_json_document_carries_a_boolean_paired() {
        let document = scanner_view::json(&scanner("mock_usb_1", false));
        assert_eq!(document["Paired"], false);
        assert_eq!(document["Id"], "mock_usb_1");
    }
}
