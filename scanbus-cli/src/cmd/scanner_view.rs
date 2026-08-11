//! What `list`, `show` and `discover` share: a [`ScannerState`] read off the bus, and
//! the two ways it is shown.
//!
//! One row shape and one JSON shape for all three commands, because a scanner does not
//! become a different object depending on which command found it — `discover`'s table
//! is [`scanbus-cli.md`] §4's example, and it is also what `list` prints for the
//! scanners a restarted daemon already knew about.
//!
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

use scanbus_client::{Connection, Error as ClientError, ScannerState};

use crate::context::Context;
use crate::error::{Error, Result};

/// The columns `list` and `discover` share, in the order §4's example prints them.
pub const HEADERS: [&str; 5] = ["BACKEND", "ID", "NAME", "STATUS", "PAIRED"];

/// One scanner, as a table row under [`HEADERS`].
pub fn row(state: &ScannerState) -> Vec<String> {
    vec![
        state.backend.clone(),
        state.id.as_str().to_owned(),
        state.name.clone(),
        state.status.as_str().to_owned(),
        (if state.paired { "yes" } else { "no" }).to_owned(),
    ]
}

/// One scanner, with every `Scanner1` property named exactly as §6 requires.
///
/// `Capabilities` serialises through [`scanbus_core::Capabilities`]'s own `Serialize`,
/// which already spells `color_modes`/`sources`/`buttons` the way the property does —
/// so an unknown key a backend reported keeps reaching `--json` unchanged, the same
/// promise [`crate::convert`] makes for the wire in the other direction.
pub fn json(state: &ScannerState) -> serde_json::Value {
    serde_json::json!({
        "Id": state.id.as_str(),
        "Name": state.name,
        "Backend": state.backend,
        "Address": state.address,
        "Capabilities": state.capabilities,
        "SupportedProfiles": state
            .supported_profiles
            .iter()
            .map(|profile| profile.as_str())
            .collect::<Vec<_>>(),
        "Paired": state.paired,
        "Connected": state.connected,
        "Status": state.status.as_str(),
        "DefaultProfile": state.default_profile.map_or("", |profile| profile.as_str()),
        "PairingState": state.pairing.as_str(),
        "PairingError": state.pairing.pairing_error(),
    })
}

/// Reads one scanner's state, turning "it just left the bus" into `Ok(None)` rather
/// than a failure.
///
/// The snapshot [`scanbus_client::Objects`] resolves against is a photograph — an
/// unpaired scanner's lifetime is bounded by the discovery session that found it
/// ([`scanbus-cli.md`] §5) — so a scanner every one of these commands names by walking
/// that snapshot can legitimately be gone by the time this call reaches it. That is not
/// a reason for `list` to fail on everything *else* it found.
///
/// # Errors
///
/// [`Error`] for anything else the call ends as: a refusal, a value this client cannot
/// decode, or `--timeout` elapsing.
pub async fn fetch(
    context: &Context,
    connection: &Connection,
    scanner: &scanbus_client::Scanner,
) -> Result<Option<ScannerState>> {
    let what = format!("reading scanner {}", scanner.id);

    match tokio::time::timeout(context.timeout, ScannerState::fetch(connection, &scanner.id)).await
    {
        Ok(Ok(state)) => Ok(Some(state)),
        Ok(Err(error)) => match scanner.gone(error) {
            ClientError::Vanished { .. } => Ok(None),
            other => Err(Error::call(what, other)),
        },
        Err(_elapsed) => Err(Error::timeout(what, context.timeout)),
    }
}
