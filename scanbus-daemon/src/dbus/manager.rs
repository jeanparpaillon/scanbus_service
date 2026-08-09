//! [`Manager1`]: the three methods of §2, at `/org/scanbus`.
//!
//! Thin on purpose. `StartDiscovery` and `StopDiscovery` are a session
//! ([`crate::discovery`]) and everything they publish reaches clients through the
//! `ObjectManager` on the same object — §2 says so explicitly: no `ScannerFound` signal,
//! because `InterfacesAdded` already is one. What is left here is argument handling,
//! which is the part with a contract of its own:
//!
//! - **A `filters` this daemon does not understand is an error**, not a warning. §2
//!   documents one key, `backends`, holding an array of ids. A typo in either the key
//!   or an id would otherwise have `StartDiscovery` succeed while probing something
//!   else entirely, and the client would wait for scanners that were never looked for.
//! - **A backend that fails to probe is not an error** — see [`crate::discovery`]. The
//!   asymmetry is deliberate: one is a client bug, the other is the machine.
//!
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md

use std::collections::HashMap;
use std::sync::Arc;

use scanbus_core::ProfileKind;
use tracing::{info, instrument};
use zbus::fdo;
use zbus::zvariant::{OwnedValue, Value};

use crate::discovery::Discovery;

/// The `filters` key of §2: `{"backends": ["sane","avahi"]}`.
const BACKENDS_FILTER: &str = "backends";

/// The `org.scanbus.Manager1` object of §2.
pub struct Manager1 {
    discovery: Arc<Discovery>,
}

impl Manager1 {
    /// A manager driving `discovery`.
    pub fn new(discovery: Arc<Discovery>) -> Self {
        Self { discovery }
    }
}

#[zbus::interface(name = "org.scanbus.Manager1")]
impl Manager1 {
    /// Starts discovery through every active backend, or the subset `filters` names.
    ///
    /// Returns as soon as the session is running — the scanners arrive afterwards, as
    /// `InterfacesAdded` for objects with `Paired=false`.
    ///
    /// # Errors
    ///
    /// `org.freedesktop.DBus.Error.InvalidArgs` for a `filters` map this daemon cannot
    /// honour: an unknown key, a `backends` value that is not an array of strings, or a
    /// backend id that does not exist.
    #[instrument(level = "info", skip_all)]
    async fn start_discovery(&self, filters: HashMap<String, OwnedValue>) -> fdo::Result<()> {
        let backends = backend_filter(&filters)?;

        self.discovery
            .start(backends.as_deref())
            .await
            .map_err(|error| fdo::Error::InvalidArgs(error.to_string()))
    }

    /// Stops the discovery in progress.
    ///
    /// Returns once the unpaired scanner objects are off the bus, so the
    /// `InterfacesRemoved` signals precede the method reply. Succeeds when no session
    /// is running.
    #[instrument(level = "info", skip_all)]
    async fn stop_discovery(&self) {
        self.discovery.stop().await;
    }

    /// The profile types this daemon can actually run.
    ///
    /// `["image","document"]` in this iteration, not the four of §2: advertising `email`
    /// or `ocr` before [3.1](https://github.com/jeanparpaillon/scanbus_service/issues/13)
    /// implements them would have a UI offer a profile that fails at scan time, and
    /// `Button1.Profile` accepts what this returns.
    ///
    /// Derived from [`ProfileKind::is_supported`] rather than from the exported
    /// `Profile1` objects, which do not exist yet; 3.1 moves the source without moving
    /// the answer.
    fn get_profile_types(&self) -> Vec<String> {
        profile_types()
    }
}

/// The profile names `GetProfileTypes` answers with.
fn profile_types() -> Vec<String> {
    ProfileKind::supported()
        .into_iter()
        .map(|kind| kind.as_str().to_owned())
        .collect()
}

/// Reads the `backends` selection out of a `filters` map.
///
/// `None` means "every backend", which is both an absent key and an absent map.
///
/// # Errors
///
/// `InvalidArgs` for an unknown key or a `backends` value that is not `as`.
fn backend_filter(filters: &HashMap<String, OwnedValue>) -> fdo::Result<Option<Vec<String>>> {
    for key in filters.keys() {
        if key != BACKENDS_FILTER {
            return Err(fdo::Error::InvalidArgs(format!(
                "unknown discovery filter {key:?}; this daemon understands {BACKENDS_FILTER:?}"
            )));
        }
    }

    let Some(value) = filters.get(BACKENDS_FILTER) else {
        return Ok(None);
    };

    let names = string_array(value).ok_or_else(|| {
        fdo::Error::InvalidArgs(format!(
            "the {BACKENDS_FILTER:?} filter must be an array of strings, got signature {}",
            value.value_signature()
        ))
    })?;

    info!(?names, "backend filter requested");
    Ok(Some(names))
}

/// The strings in `value`, whether it holds an `as` or an `av` of strings.
///
/// Both spellings reach us in practice: `busctl … 'a{sv}' 1 backends as 1 sane` sends
/// the first, while a client that builds its variants one at a time sends the second.
/// Refusing the second would be a distinction no client can see in its own source.
fn string_array(value: &OwnedValue) -> Option<Vec<String>> {
    if let Ok(names) = Vec::<String>::try_from(value.clone()) {
        return Some(names);
    }

    let items = Vec::<Value<'_>>::try_from(value.clone()).ok()?;
    items
        .into_iter()
        .map(|item| String::try_from(item).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filters(entries: [(&str, OwnedValue); 1]) -> HashMap<String, OwnedValue> {
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect()
    }

    fn owned(value: Value<'_>) -> OwnedValue {
        OwnedValue::try_from(value).unwrap()
    }

    #[test]
    fn no_filter_means_every_backend() {
        assert_eq!(backend_filter(&HashMap::new()).unwrap(), None);
    }

    /// The spelling from §2, and the one a client that builds `av` sends.
    #[test]
    fn both_spellings_of_an_array_of_names_are_read() {
        let as_strings = owned(Value::from(vec!["sane".to_owned(), "avahi".to_owned()]));
        assert_eq!(
            backend_filter(&filters([(BACKENDS_FILTER, as_strings)])).unwrap(),
            Some(vec!["sane".to_owned(), "avahi".to_owned()])
        );

        let as_variants = owned(Value::from(vec![Value::from("sane"), Value::from("avahi")]));
        assert_eq!(
            backend_filter(&filters([(BACKENDS_FILTER, as_variants)])).unwrap(),
            Some(vec!["sane".to_owned(), "avahi".to_owned()])
        );
    }

    #[test]
    fn a_backends_value_of_the_wrong_type_is_invalid_args() {
        let error = backend_filter(&filters([(BACKENDS_FILTER, owned(Value::from(42u32)))]))
            .expect_err("a u32 is not a list of backend names");

        assert!(matches!(error, fdo::Error::InvalidArgs(_)), "{error:?}");
        assert!(error.to_string().contains("array of strings"), "{error}");
    }

    /// A typo in the key would otherwise probe everything and look like it worked.
    #[test]
    fn an_unknown_filter_key_is_invalid_args() {
        let error = backend_filter(&filters([("backend", owned(Value::from(vec!["sane"])))]))
            .expect_err("an unknown key must not be ignored");

        assert!(matches!(error, fdo::Error::InvalidArgs(_)), "{error:?}");
        assert!(error.to_string().contains("backend"), "{error}");
    }

    /// The list this iteration can honour, and the reason `GetProfileTypes` is not §2's
    /// four-element list.
    #[test]
    fn only_the_implemented_profiles_are_advertised() {
        assert_eq!(profile_types(), ["image", "document"]);
    }
}
