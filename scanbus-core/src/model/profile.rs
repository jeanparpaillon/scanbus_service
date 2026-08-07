//! [`ProfileKind`]: the four post-processing profiles.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::ParseError;

/// A post-processing profile — `image`, `document`, `email` or `ocr` (§6).
///
/// All four are in the type although only two are implemented, because they are all in
/// the API: `GetProfileTypes` advertises the four, a `Button1.Profile` may already be
/// set to `"email"` by a config UI, and the pairing store may hold one written by a
/// later version. Leaving `Email` and `Ocr` out would turn every one of those into a
/// parse error indistinguishable from a typo, when the honest answer is
/// `org.scanbus.Error.UnsupportedProfile` (2.7) — so they parse, and
/// [`ProfileKind::is_supported`] is what refuses them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileKind {
    /// One file per page (jpeg/png).
    Image,
    /// A single multi-page PDF.
    Document,
    /// A mail draft with the scan attached — not implemented yet.
    Email,
    /// Text recognition, as text or a searchable PDF — not implemented yet.
    Ocr,
}

impl ProfileKind {
    /// Every variant, in the order `GetProfileTypes` returns them (§2).
    pub const ALL: [ProfileKind; 4] = [Self::Image, Self::Document, Self::Email, Self::Ocr];

    /// The exact string used in `Profile`, `SupportedProfiles` and the profile paths.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Document => "document",
            Self::Email => "email",
            Self::Ocr => "ocr",
        }
    }

    /// Whether this daemon can actually run the profile.
    ///
    /// `false` for [`ProfileKind::Email`] and [`ProfileKind::Ocr`] until their
    /// processors exist; the pipeline is `image` + `document` only
    /// ([`scanbus-rust-implementation.md`] §6).
    ///
    /// [`scanbus-rust-implementation.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-rust-implementation.md
    pub const fn is_supported(&self) -> bool {
        matches!(self, Self::Image | Self::Document)
    }

    /// Parses the `Button1.Profile` / `Job1.Profile` rendering, where `""` means
    /// unassigned (§5) rather than invalid.
    ///
    /// # Errors
    ///
    /// Fails on any non-empty string that is not one of the four documented names.
    pub fn parse_optional(s: &str) -> Result<Option<Self>, ParseError> {
        if s.is_empty() {
            return Ok(None);
        }
        s.parse().map(Some)
    }

    /// The inverse of [`ProfileKind::parse_optional`]: `None` renders as `""`.
    pub const fn optional_as_str(profile: Option<Self>) -> &'static str {
        match profile {
            Some(kind) => kind.as_str(),
            None => "",
        }
    }
}

impl fmt::Display for ProfileKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProfileKind {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == s)
            .ok_or_else(|| ParseError::new("ProfileKind", s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_documented_strings() {
        let expected = ["image", "document", "email", "ocr"];

        for (kind, string) in ProfileKind::ALL.into_iter().zip(expected) {
            assert_eq!(kind.as_str(), string);
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{string}\"")
            );
            assert_eq!(string.parse::<ProfileKind>().unwrap(), kind);
            assert_eq!(
                serde_json::from_str::<ProfileKind>(&format!("\"{string}\"")).unwrap(),
                kind
            );
        }
    }

    /// The unimplemented profiles must parse — a config UI can set them — and must not
    /// claim to be runnable.
    #[test]
    fn email_and_ocr_parse_but_are_unsupported() {
        assert_eq!("email".parse::<ProfileKind>().unwrap(), ProfileKind::Email);
        assert_eq!("ocr".parse::<ProfileKind>().unwrap(), ProfileKind::Ocr);

        assert!(ProfileKind::Image.is_supported());
        assert!(ProfileKind::Document.is_supported());
        assert!(!ProfileKind::Email.is_supported());
        assert!(!ProfileKind::Ocr.is_supported());
    }

    #[test]
    fn the_empty_string_means_unassigned_not_invalid() {
        assert_eq!(ProfileKind::parse_optional("").unwrap(), None);
        assert_eq!(
            ProfileKind::parse_optional("ocr").unwrap(),
            Some(ProfileKind::Ocr)
        );
        assert!(ProfileKind::parse_optional("pdf").is_err());

        assert_eq!(ProfileKind::optional_as_str(None), "");
        assert_eq!(
            ProfileKind::optional_as_str(Some(ProfileKind::Document)),
            "document"
        );
    }
}
