//! The four panel entries, and the three numberings that name them.
//!
//! One table, because three different parts of the crate need it and each needs a
//! different column: [`register`](super::register) puts `FUNC` and `APPNUM` on the wire,
//! [`event`](super::event) turns a `FUNC` back into the entry that was pressed, and the
//! backend publishes `DeviceLabel` and `buttons.count` from it. Keeping it here rather
//! than in `register` is what makes "the registration and the key press agree about
//! button 1" a property of one `match` rather than of two that happen to line up.
//!
//! The table is the arch docs': `scanbus-dbus-api.md` §5's worked example fixes index ↔
//! `DeviceLabel`, and `brother-skeyless-backend.md` §3 adds `FUNC` and `APPNUM` to it.
//! `tests/arch_button_table.rs` reads both of those markdown tables and asserts this
//! module against them, so the docs are the source and this file is the copy.

use std::fmt;

/// What a panel entry does when it is chosen.
///
/// The three numbers attached to each are all different and all load-bearing, which is
/// why they are methods here rather than something a call site works out:
/// [`Function::appnum`] goes on the wire, [`Function::button_index`] is the scanbus API's
/// `Button1` index (§5), and the vendor's own `decode_key_data` uses a *third* order
/// (IMAGE 0, OCR 1, EMAIL 2, FILE 3) internally, which this crate does not adopt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Function {
    File,
    Image,
    Ocr,
    Email,
}

impl Function {
    /// Every function, in `button_index` order.
    pub const ALL: [Self; 4] = [Self::File, Self::Image, Self::Ocr, Self::Email];

    /// The `FUNC=` token.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::File => "FILE",
            Self::Image => "IMAGE",
            Self::Ocr => "OCR",
            Self::Email => "EMAIL",
        }
    }

    /// The `APPNUM=` token. Not an index — the values are 1, 3, 2, 5 and skip 4.
    pub const fn appnum(self) -> u8 {
        match self {
            Self::Image => 1,
            Self::Email => 2,
            Self::Ocr => 3,
            Self::File => 5,
        }
    }

    /// The scanbus `Button1` index, per the table in `brother-skeyless-backend.md` §3.
    pub const fn button_index(self) -> u32 {
        match self {
            Self::File => 0,
            Self::Image => 1,
            Self::Ocr => 2,
            Self::Email => 3,
        }
    }

    /// What the panel calls this entry — the `DeviceLabel` of API §5.
    ///
    /// The firmware's own wording, spelled as the API's worked example spells it:
    /// "Scan to E-mail", not the "E-Mail" the vendor daemon's own strings use. A client
    /// that matches on the label — the GUI's per-key hint, for one — matches the
    /// contract, so the contract wins. `LabelConfigurable` stays `false` either way:
    /// nothing scanbus sends changes what the LCD shows.
    pub const fn device_label(self) -> &'static str {
        match self {
            Self::File => "Scan to File",
            Self::Image => "Scan to Image",
            Self::Ocr => "Scan to OCR",
            Self::Email => "Scan to E-mail",
        }
    }

    pub fn from_button_index(index: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.button_index() == index)
    }

    pub fn from_appnum(appnum: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.appnum() == appnum)
    }

    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == token)
    }
}

impl fmt::Display for Function {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
