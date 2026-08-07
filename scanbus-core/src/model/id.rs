//! [`ScannerId`]: the one field with a hard external contract.
//!
//! It keys the pairing store (4.1) and appears in every object path, so it has to be
//! stable across reboots *and* legal as a D-Bus path element — which allows only
//! `[A-Za-z0-9_]`, no dots, no dashes, no colons.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::InvalidScannerId;

/// A scanner identifier, valid as a D-Bus object-path element.
///
/// Construct it from backend-supplied strings with [`ScannerId::from_backend`], which
/// escapes; use [`ScannerId::new`] only for a string that is already in the charset
/// (reading one back from the pairing store, say — where it is re-validated).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ScannerId(String);

impl ScannerId {
    /// Longest accepted id.
    ///
    /// A D-Bus object path may not exceed 255 bytes in total, and the longest path an
    /// id appears in is `/org/scanbus/scanner/{id}/button/{n}`. Refusing here turns
    /// what would be a runtime rejection by the bus — at the moment the object is
    /// exported, far from the backend that produced the name — into a discovery error.
    pub const MAX_LEN: usize = 200;

    /// Wraps a string that is already a legal path element.
    ///
    /// # Errors
    ///
    /// Fails on an empty string, on anything longer than [`ScannerId::MAX_LEN`], and on
    /// any character outside `[A-Za-z0-9_]` — including the `.`, `-`, `:` and `/` that
    /// backend addresses are full of.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidScannerId> {
        let value = value.into();

        let reason = if value.is_empty() {
            Some("empty")
        } else if value.len() > Self::MAX_LEN {
            Some("longer than 200 bytes, which would overflow the object path")
        } else if !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            Some("contains a character outside the D-Bus path charset [A-Za-z0-9_]")
        } else {
            None
        };

        match reason {
            Some(reason) => Err(InvalidScannerId { value, reason }),
            None => Ok(Self(value)),
        }
    }

    /// Derives an id from the backend that found the scanner and the scanner's address.
    ///
    /// `backend` is one of ours (`"sane"`, `"escl"`, `"brother"`) and must match
    /// `[a-z0-9]+`; `address` is whatever the backend reports and is escaped with
    /// [`escape_component`].
    ///
    /// The result diverges from the illustrative `sane_epson2_net_192_168_1_50` in
    /// [`scanbus-dbus-api.md`] §1, and on purpose. Substituting `_` for every illegal
    /// character is not injective: `192.168.1.50` and a device whose address really is
    /// `192_168_1_50` collapse onto the same id, and two scanners on one path is a
    /// silent, permanent mix-up of two pairings. Escaping costs readability
    /// (`sane_epson2_3Anet_3A192_2E168_2E1_2E50`) and buys the guarantee that distinct
    /// inputs give distinct ids — see the tests in this module.
    ///
    /// Restricting `backend` to lowercase alphanumerics is what makes the join
    /// unambiguous: the escaped address may contain `_`, but the backend part never
    /// does, so the first `_` is always the separator.
    ///
    /// # Errors
    ///
    /// Fails if `backend` is empty or contains anything but `[a-z0-9]`, or if the
    /// escaped result exceeds [`ScannerId::MAX_LEN`].
    ///
    /// [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md
    pub fn from_backend(backend: &str, address: &str) -> Result<Self, InvalidScannerId> {
        if backend.is_empty()
            || !backend
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        {
            return Err(InvalidScannerId {
                value: backend.to_owned(),
                reason: "backend id must be non-empty and match [a-z0-9]",
            });
        }

        Self::new(format!("{backend}_{}", escape_component(address)))
    }

    /// The id as it appears in an object path.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ScannerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ScannerId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ScannerId {
    type Error = InvalidScannerId;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ScannerId> for String {
    fn from(id: ScannerId) -> Self {
        id.0
    }
}

/// Escapes an arbitrary string into the D-Bus path charset, injectively.
///
/// `[A-Za-z0-9]` passes through; every other byte — `_` included, as `_5F` — becomes
/// `_` followed by two uppercase hex digits. Since `_` never survives unescaped, each
/// `_` in the output starts exactly one three-character escape, which is what makes the
/// mapping reversible ([`unescape_component`]) and therefore collision-free.
pub fn escape_component(raw: &str) -> String {
    use fmt::Write as _;

    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() {
            out.push(char::from(byte));
        } else {
            // Infallible: writing into a String.
            let _ = write!(out, "_{byte:02X}");
        }
    }
    out
}

/// Reverses [`escape_component`].
///
/// Returns `None` if `escaped` is not something [`escape_component`] could have
/// produced: a truncated or non-hex escape, or bytes that do not form valid UTF-8.
/// Mostly of interest to tests and to log messages that want the address back.
pub fn unescape_component(escaped: &str) -> Option<String> {
    let mut out = Vec::with_capacity(escaped.len());
    let mut bytes = escaped.bytes();

    while let Some(byte) = bytes.next() {
        if byte != b'_' {
            out.push(byte);
            continue;
        }

        let hi = char::from(bytes.next()?).to_digit(16)?;
        let lo = char::from(bytes.next()?).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }

    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_accepts_the_path_charset() {
        assert_eq!(
            ScannerId::new("sane_epson2_3Anet").unwrap().as_str(),
            "sane_epson2_3Anet"
        );
    }

    #[test]
    fn new_rejects_everything_outside_it() {
        for bad in [
            "epson2.net",
            "epson2/net",
            "brother-skey",
            "epson 2",
            "epson2:net",
            "",
        ] {
            assert!(
                ScannerId::new(bad).is_err(),
                "{bad:?} should not be a scanner id"
            );
        }
    }

    #[test]
    fn new_rejects_an_id_that_would_overflow_the_object_path() {
        let long = "a".repeat(ScannerId::MAX_LEN + 1);
        assert!(ScannerId::new(long).is_err());
        assert!(ScannerId::new("a".repeat(ScannerId::MAX_LEN)).is_ok());
    }

    #[test]
    fn from_backend_escapes_the_address() {
        let id = ScannerId::from_backend("sane", "epson2:net:192.168.1.50").unwrap();
        assert_eq!(id.as_str(), "sane_epson2_3Anet_3A192_2E168_2E1_2E50");
        assert!(ScannerId::new(id.as_str()).is_ok());
    }

    #[test]
    fn from_backend_rejects_a_backend_id_it_cannot_separate() {
        for bad in ["", "brother-skey", "Brother", "brother_skey"] {
            assert!(
                ScannerId::from_backend(bad, "usb").is_err(),
                "{bad:?} should not be a backend id"
            );
        }
    }

    /// The point of escaping rather than substituting: no two addresses share an id.
    #[test]
    fn distinct_addresses_give_distinct_ids() {
        let addresses = [
            "192.168.1.50",
            // Substituting `_` for `.` would collapse this onto the previous one.
            "192_168_1_50",
            "192.168.1.5",
            "192.168.15.0",
            "epson2:net:192.168.1.50",
            "epson2 net 192.168.1.50",
            "usb:001:002",
            "usb:001:003",
            "",
            "_",
            "__",
            "_2E",
            ".",
            "Épson",
        ];

        let mut ids: Vec<String> = addresses
            .iter()
            .map(|address| {
                ScannerId::from_backend("sane", address)
                    .unwrap()
                    .as_str()
                    .to_owned()
            })
            .collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();

        assert_eq!(ids.len(), count, "two addresses collided onto one id");
    }

    /// And no two (backend, address) pairs do either, which is what the `_` join has to
    /// preserve: the escaped address may contain `_`, the backend id may not.
    #[test]
    fn distinct_backend_and_address_pairs_give_distinct_ids() {
        let pairs = [
            ("sane", "epson"),
            ("sane", "_epson"),
            ("saneepson", ""),
            ("sane2", "epson"),
            ("escl", "epson"),
        ];

        let mut ids: Vec<String> = pairs
            .iter()
            .map(|(backend, address)| {
                ScannerId::from_backend(backend, address)
                    .unwrap()
                    .as_str()
                    .to_owned()
            })
            .collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();

        assert_eq!(ids.len(), count, "two (backend, address) pairs collided");
    }

    #[test]
    fn escaping_round_trips() {
        for raw in [
            "epson2:net:192.168.1.50",
            "192_168_1_50",
            "",
            "_",
            "Épson — flatbed",
            "\n\t",
        ] {
            let escaped = escape_component(raw);
            assert!(
                escaped
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                "{escaped:?} is not a legal path element"
            );
            assert_eq!(unescape_component(&escaped).as_deref(), Some(raw));
        }
    }

    #[test]
    fn unescape_rejects_what_escape_cannot_produce() {
        for bad in ["_", "_2", "_ZZ", "a_2"] {
            assert_eq!(unescape_component(bad), None, "{bad:?} should not decode");
        }
    }

    #[test]
    fn serde_round_trips_and_validates_on_the_way_in() {
        let id = ScannerId::from_backend("sane", "epson2:net:192.168.1.50").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{}\"", id.as_str()));
        assert_eq!(serde_json::from_str::<ScannerId>(&json).unwrap(), id);

        // A pairing store edited by hand cannot smuggle in an illegal path element.
        assert!(serde_json::from_str::<ScannerId>("\"epson2.net\"").is_err());
    }
}
