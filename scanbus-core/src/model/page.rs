//! [`RawPage`]: one page as the backend hands it over.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A single scanned page, before any profile has touched it.
///
/// This is what `fetch_pages` streams ([`scanbus-rust-implementation.md`] §3) and what a
/// `ProfileProcessor` consumes: the image profile writes each one straight to a file,
/// the document profile buffers them all and assembles a PDF (§6).
///
/// Deliberately not `Serialize`: a page is bytes in flight between a backend and a
/// processor, never persisted and never on the bus — `Job1.Result` carries paths, not
/// pixels. Deriving serialisation here would invite someone to put a scan through JSON.
///
/// [`scanbus-rust-implementation.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-rust-implementation.md
#[derive(Clone, PartialEq, Eq)]
pub struct RawPage {
    /// Position in the job, 0-based; pages of an ADF batch arrive in order.
    pub index: u32,
    /// Encoding of [`RawPage::data`].
    pub format: PageFormat,
    /// Resolution the page was captured at, in DPI.
    pub resolution_dpi: u32,
    /// The encoded page.
    pub data: Vec<u8>,
}

impl fmt::Debug for RawPage {
    /// Prints the size of the page rather than the page: a debug log of a scan should
    /// stay readable.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawPage")
            .field("index", &self.index)
            .field("format", &self.format)
            .field("resolution_dpi", &self.resolution_dpi)
            .field("data", &format_args!("{} bytes", self.data.len()))
            .finish()
    }
}

/// Encoding of the bytes a backend produces.
///
/// SANE-derived backends hand over PNM; eSCL and most network paths already produce
/// JPEG. The profile pipeline converts from here, so the variant has to be known rather
/// than guessed from the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PageFormat {
    /// Netpbm, what the SANE tooling writes by default.
    Pnm,
    /// JPEG.
    Jpeg,
    /// PNG.
    Png,
    /// TIFF.
    Tiff,
}

impl PageFormat {
    /// Every variant.
    pub const ALL: [PageFormat; 4] = [Self::Pnm, Self::Jpeg, Self::Png, Self::Tiff];

    /// Lowercase name, as used in profile options and log lines.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pnm => "pnm",
            Self::Jpeg => "jpeg",
            Self::Png => "png",
            Self::Tiff => "tiff",
        }
    }

    /// Media type, for the file the profile writes.
    pub const fn mime_type(self) -> &'static str {
        match self {
            Self::Pnm => "image/x-portable-anymap",
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Tiff => "image/tiff",
        }
    }

    /// Usual file extension.
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Pnm => "pnm",
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::Tiff => "tiff",
        }
    }
}

impl fmt::Display for PageFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_reports_the_size_not_the_pixels() {
        let page = RawPage {
            index: 0,
            format: PageFormat::Pnm,
            resolution_dpi: 300,
            data: vec![0xff; 4096],
        };

        let rendered = format!("{page:?}");
        assert!(rendered.contains("4096 bytes"), "{rendered}");
        assert!(!rendered.contains("255"), "{rendered}");
    }

    #[test]
    fn formats_round_trip() {
        for format in PageFormat::ALL {
            let json = serde_json::to_string(&format).unwrap();
            assert_eq!(json, format!("\"{}\"", format.as_str()));
            assert_eq!(
                serde_json::from_str::<PageFormat>(&json).unwrap(),
                format,
                "{format} did not round-trip"
            );
        }
    }

    #[test]
    fn jpeg_extension_is_the_three_letter_one() {
        assert_eq!(PageFormat::Jpeg.as_str(), "jpeg");
        assert_eq!(PageFormat::Jpeg.extension(), "jpg");
        assert_eq!(PageFormat::Jpeg.mime_type(), "image/jpeg");
    }
}
