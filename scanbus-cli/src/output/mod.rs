//! How a result reaches the terminal: which format, and whether it is coloured.
//!
//! Only the two decisions every command shares live here. The column-aligned table and
//! the JSON Lines renderer of [`scanbus-cli.md`] §6 are [8.4]; what this module owes them
//! is a [`Format`] to branch on and a [`Style`] that has already worked out whether ANSI
//! escapes are allowed, because that question has four inputs and getting it wrong is
//! invisible until someone pipes the output into a file.
//!
//! [8.4]: https://github.com/jeanparpaillon/scanbus_service/issues/31
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

use std::io::{IsTerminal as _, Write};

use crate::error::{Error, Result};

/// What stands in for an empty value in human output (§6).
pub const EMPTY: &str = "-";

/// Human table or machine JSON — the `--json` flag of §3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Column-aligned text for a person.
    Human,
    /// One JSON document, or one object per line for the streaming commands.
    Json,
}

impl Format {
    /// The format `--json` selects.
    pub const fn new(json: bool) -> Self {
        if json { Self::Json } else { Self::Human }
    }
}

/// Whether output may carry ANSI escapes, and the escapes themselves.
///
/// Four inputs, in the order §3 gives them: `--no-color`, then `NO_COLOR`, then "stdout
/// is not a terminal", then on. `--json` is a fifth and takes precedence over all of
/// them — an escape sequence inside a JSON string is not something `jq` recovers from,
/// so the flag implies `--no-color` rather than merely suggesting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    color: bool,
}

// Two of these are reached only from tests today: the renderers that will ask a `Style`
// whether it is painting, and construct a plain one to assert against, are 8.4's. They
// are part of the type's surface rather than test scaffolding, so they stay here.
#[allow(
    dead_code,
    reason = "PLAIN and enabled() are the table renderer's half of this type — 8.4"
)]
impl Style {
    /// Plain text, no escapes. What a test asserts against.
    pub const PLAIN: Self = Self { color: false };

    /// The decision, made from the process's own environment.
    pub fn detect(no_color: bool, format: Format) -> Self {
        Self::resolve(
            no_color,
            format,
            std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()),
            std::io::stdout().is_terminal(),
        )
    }

    /// The same decision with its inputs passed in, which is the testable half.
    ///
    /// `NO_COLOR` counts when it is set to anything non-empty, per the convention's own
    /// wording — `NO_COLOR=0` still means no colour, and treating it as a boolean is the
    /// classic misreading.
    pub const fn resolve(no_color: bool, format: Format, no_color_env: bool, tty: bool) -> Self {
        let color = !no_color && !no_color_env && tty && matches!(format, Format::Human);
        Self { color }
    }

    /// Whether escapes are being emitted.
    pub const fn enabled(self) -> bool {
        self.color
    }

    /// Bold, for a column header or a field label.
    pub fn bold(self, text: &str) -> String {
        self.sgr("1", text)
    }

    /// Green: the state a user hopes for.
    pub fn good(self, text: &str) -> String {
        self.sgr("32", text)
    }

    /// Yellow: not wrong, not ready either.
    pub fn warn(self, text: &str) -> String {
        self.sgr("33", text)
    }

    /// Red: the thing the command was about is not there.
    pub fn bad(self, text: &str) -> String {
        self.sgr("31", text)
    }

    /// One SGR pair around `text`, or `text` untouched when colour is off.
    ///
    /// The reset is `0m` rather than the specific off-code, because these nest in
    /// practice (a bold label inside a coloured row) and the specific codes do not
    /// compose the way the generic reset does.
    fn sgr(self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
}

/// Writes one JSON document and a newline.
///
/// A newline even for the single-shot commands, so that `--json` output is a valid line
/// in a pipeline that also carries the JSON Lines of a streaming command.
///
/// # Errors
///
/// [`Error::write`] when stdout cannot be written — a closed pipe, in practice, which is
/// what `scanbus … | head` does and which must not panic.
pub fn json(writer: &mut impl Write, value: &serde_json::Value) -> Result<()> {
    writeln!(writer, "{value}").map_err(Error::write)
}

/// Writes a label/value block: labels padded to one column, `-` for an empty value.
///
/// The shape `status` and `show` share, and the reason it is not the table renderer of
/// [8.4]: there is one object here, so the columns are the *properties*, not the rows.
///
/// # Errors
///
/// [`Error::write`], as above.
///
/// [8.4]: https://github.com/jeanparpaillon/scanbus_service/issues/31
pub fn fields(writer: &mut impl Write, style: Style, rows: &[(&str, String)]) -> Result<()> {
    // The escapes in a painted value must not count towards the column width, so the
    // width comes from the labels — which are never painted with anything but bold, and
    // are written last on each line anyway.
    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);

    for (label, value) in rows {
        let value = if value.is_empty() { EMPTY } else { value };
        let padding = " ".repeat(width - label.len());
        writeln!(writer, "{}{padding}  {value}", style.bold(label)).map_err(Error::write)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The precedence of §3, one row per input that can turn colour off.
    #[test]
    fn colour_is_on_only_on_a_terminal_that_nothing_has_vetoed() {
        // no_color, format, NO_COLOR, tty -> coloured?
        let cases = [
            (false, Format::Human, false, true, true),
            (true, Format::Human, false, true, false),
            (false, Format::Human, true, true, false),
            (false, Format::Human, false, false, false),
            (false, Format::Json, false, true, false),
        ];

        for (no_color, format, env, tty, expected) in cases {
            let style = Style::resolve(no_color, format, env, tty);
            assert_eq!(
                style.enabled(),
                expected,
                "--no-color={no_color} {format:?} NO_COLOR={env} tty={tty}"
            );
        }
    }

    /// `--json` implies `--no-color`: an escape inside a JSON string is not recoverable
    /// by the consumer the flag exists for.
    #[test]
    fn json_is_never_coloured_even_on_a_terminal() {
        let style = Style::resolve(false, Format::Json, false, true);
        assert_eq!(style.bold("running"), "running");
    }

    #[test]
    fn a_plain_style_emits_no_escapes_and_a_coloured_one_does() {
        assert_eq!(Style::PLAIN.good("running"), "running");
        assert_eq!(
            Style::resolve(false, Format::Human, false, true).good("running"),
            "\x1b[32mrunning\x1b[0m"
        );
    }

    #[test]
    fn fields_pad_the_labels_and_stand_in_for_an_empty_value() {
        let mut out = Vec::new();
        fields(
            &mut out,
            Style::PLAIN,
            &[("daemon", "running".to_owned()), ("version", String::new())],
        )
        .unwrap();

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "daemon   running\nversion  -\n"
        );
    }

    #[test]
    fn a_json_document_is_one_line() {
        let mut out = Vec::new();
        json(&mut out, &serde_json::json!({"daemon": "running"})).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"daemon\":\"running\"}\n"
        );
    }
}
