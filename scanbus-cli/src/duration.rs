//! `--timeout 30s`, `--for 10s`: the one number format the CLI reads.
//!
//! Hand-written rather than delegated to a general-purpose duration crate, for the same
//! reason [`scanbus_client::Bus`] parses three spellings and no more: the grammar
//! [`scanbus-cli.md`] §3 uses is *one* number and *one* unit, and a parser that also
//! accepted `1h 30m 5s` would be documenting a syntax no option in this CLI has a use
//! for. The error message is the other half — a bad `--timeout` must name the units it
//! knows, because "invalid value" leaves the user guessing between `30`, `30s` and
//! `30 seconds`.
//!
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

use std::time::Duration;

/// Units, longest suffix first so that `ms` is not read as `m`.
const UNITS: &[(&str, u64)] = &[("ms", 1), ("s", 1_000), ("m", 60_000), ("h", 3_600_000)];

/// Reads `500ms`, `30s`, `2m`, `1h`, or a bare number of seconds.
///
/// A bare number is seconds because that is what every other tool in reach —
/// `timeout(1)`, `sleep(1)` — makes it, and rejecting it would be a papercut with
/// nothing behind it.
///
/// # Errors
///
/// The string as a message, naming the accepted units. `clap` prefixes it with the
/// option, so the whole line reads `error: invalid value '30x' for '--timeout
/// <DURATION>': …`.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();

    let (number, millis_per_unit) = UNITS
        .iter()
        .find_map(|(suffix, millis)| Some((text.strip_suffix(suffix)?, *millis)))
        // No suffix: seconds, and the digits are the whole string.
        .unwrap_or((text, 1_000));

    let value: u64 = number.trim().parse().map_err(|_| {
        format!(
            "expected a number followed by one of ms, s, m, h — or a bare number of \
             seconds, got {text:?}"
        )
    })?;

    // A `--timeout` of days is nonsense but not our business; an overflow is, because
    // `Duration::from_millis` would wrap it into a short timeout that then fires.
    value
        .checked_mul(millis_per_unit)
        .map(Duration::from_millis)
        .ok_or_else(|| format!("{text:?} is longer than this program can represent"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_unit_of_the_grammar_reads() {
        for (text, expected) in [
            ("500ms", Duration::from_millis(500)),
            ("30s", Duration::from_secs(30)),
            ("2m", Duration::from_secs(120)),
            ("1h", Duration::from_secs(3_600)),
            ("0s", Duration::ZERO),
        ] {
            assert_eq!(parse_duration(text).unwrap(), expected, "{text}");
        }
    }

    /// The suffix table is ordered, and this is the pair that proves it: read shortest
    /// first, `500ms` would be 500 minutes.
    #[test]
    fn milliseconds_are_not_minutes() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("500m").unwrap(), Duration::from_secs(30_000));
    }

    #[test]
    fn a_bare_number_is_seconds() {
        assert_eq!(parse_duration("30").unwrap(), Duration::from_secs(30));
    }

    /// The message is the feature: it has to say what would have worked.
    #[test]
    fn a_value_that_is_not_a_duration_names_the_units() {
        for text in ["", "s", "30x", "-5s", "1.5s", "thirty"] {
            let error = parse_duration(text).expect_err("{text} is not a duration");
            assert!(error.contains("ms, s, m, h"), "{text}: {error}");
        }
    }

    /// A multiplication that wraps would turn a huge timeout into a tiny one, which is
    /// the failure mode worth refusing rather than rounding.
    #[test]
    fn an_overflowing_duration_is_refused() {
        let error = parse_duration(&format!("{}h", u64::MAX)).expect_err("that does not fit");
        assert!(error.contains("represent"), "{error}");
    }
}
