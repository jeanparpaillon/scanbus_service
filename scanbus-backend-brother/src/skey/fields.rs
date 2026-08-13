//! The `KEY=VALUE;KEY=VALUE;` grammar both halves of the skey protocol are written in.
//!
//! The registration string sent over SNMP and the datagram the panel sends back on
//! UDP/54925 use the same shape, so they share one reader. In the vendor daemon that
//! reader is `get_token` at `0x40ba6f`: `strstr` for `"KEY="`, then copy up to the next
//! `;`.
//!
//! Two deliberate departures from `get_token`, both of which only ever *reject* input the
//! vendor would have accepted:
//!
//! - **Keys are anchored to a field boundary.** `strstr` finds `USER=` anywhere,
//!   including inside another field's value; here a key matches only at the start of a
//!   `;`-delimited field, before its first `=`. A device cannot send us a value that
//!   renames a later field.
//! - **A missing `;` terminator ends the field at the end of input** rather than running
//!   off the buffer. The vendor relies on the NUL it appends after `recvfrom`.
//!
//! A `;` inside a quoted value still separates — the vendor does no quote-aware scanning
//! either, and no observed payload contains one.
//!
//! Surrounding double quotes are stripped, because the vendor is not consistent about
//! them: `USER="%s"` in the SNMP registration format at `0x414a78`, `USER=%s` in the
//! Phoenix JSON one at `0x415a90`, and `NODENAME="BRN_9000C0"` in the sample datagram
//! compiled into `.data`.

/// The value of `key` in a `;`-separated payload, with surrounding quotes removed.
///
/// Returns `None` when the key is absent, and `Some("")` when it is present and empty —
/// `BRID=;` is a real, meaningful field, so the two cases must not collapse.
pub fn field<'a>(payload: &'a str, key: &str) -> Option<&'a str> {
    payload
        .split(';')
        .filter_map(|candidate| candidate.split_once('='))
        .find(|(name, _)| *name == key)
        .map(|(_, value)| unquote(value))
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registration string, read back field by field.
    #[test]
    fn every_field_of_a_registration_string_is_reachable() {
        let payload = "TYPE=BR;BUTTON=SCAN;USER=\"jean\";FUNC=IMAGE;\
                       HOST=192.168.1.20:54925;APPNUM=1;DURATION=360;BRID=;";
        assert_eq!(field(payload, "TYPE"), Some("BR"));
        assert_eq!(field(payload, "USER"), Some("jean"));
        assert_eq!(field(payload, "HOST"), Some("192.168.1.20:54925"));
        assert_eq!(field(payload, "APPNUM"), Some("1"));
        // Present and empty is not the same answer as absent: `BRID=;` is what "no panel
        // password" looks like on the wire, and reading it as absent would make an
        // unauthenticated registration indistinguishable from a malformed one.
        assert_eq!(field(payload, "BRID"), Some(""));
        assert_eq!(field(payload, "REGID"), None);
    }

    #[test]
    fn a_key_inside_a_value_does_not_masquerade_as_a_field() {
        // `strstr`, which is what the vendor uses, finds the `USER=` buried in NODENAME's
        // value and answers "evil". Anchoring to the field boundary answers "real".
        let payload = "NODENAME=BRW_USER=evil;USER=real";
        assert_eq!(field(payload, "USER"), Some("real"));
        assert_eq!(field(payload, "NODENAME"), Some("BRW_USER=evil"));
    }

    #[test]
    fn a_payload_without_a_trailing_semicolon_still_yields_its_last_field() {
        assert_eq!(
            field("A=1;CLIENT=192.168.1.3", "CLIENT"),
            Some("192.168.1.3")
        );
    }

    #[test]
    fn garbage_yields_nothing_rather_than_panicking() {
        for payload in ["", ";", ";;;", "=", "=;=", "NOEQUALS", "\u{1f600}=x"] {
            assert_eq!(field(payload, "USER"), None, "payload {payload:?}");
        }
        assert_eq!(field("=x", ""), Some("x"));
    }

    #[test]
    fn only_matched_quotes_are_stripped() {
        assert_eq!(field("A=\"x\"", "A"), Some("x"));
        assert_eq!(field("A=\"x", "A"), Some("\"x"));
        assert_eq!(field("A=x\"", "A"), Some("x\""));
        assert_eq!(field("A=\"\"", "A"), Some(""));
    }
}
