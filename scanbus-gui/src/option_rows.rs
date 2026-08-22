//! The six option rows — one composite-template subclass per `option-row-*.blp`.
//!
//! [`crate::options`]'s `build_row` matched on `OptionType` and built an `AdwComboRow`, an
//! `AdwSpinRow`, an `AdwSwitchRow`, an `AdwEntryRow` or an `AdwActionRow`, and `finish_row`
//! then bolted the same origin label and Reset button onto whichever came back — the same
//! suffix pair, built six times over, because Rust has no way to say "these rows all have
//! this shape". The `.blp` does, and these six types are where it says it.
//!
//! What is deliberately *not* here is everything that is not shape: which of the six a
//! schema `type` maps to, the strings a dropdown's model gets, the bounds an adjustment
//! gets, the `updating` guard and `commit`/`clear`. That stays in `options.rs`, which is
//! why nothing below knows what an `OptionsSchema`, a `Scope` or a `Value` is. A row knows
//! its own widgets and the closure the factory hung on it, and that is all.
//!
//! # Two of the six do not subclass the Adw row they look like
//!
//! `AdwSwitchRow` and `AdwSpinRow` are declared `G_DECLARE_FINAL_TYPE`, so GObject refuses
//! to derive from either — a composite template on one never registers its class, and the
//! row cannot be instantiated at all. Both are an `AdwActionRow` with one widget in the
//! suffix, so [`OptionRowFlag`] and [`OptionRowNumber`] are that tree written out; see the
//! two `.blp` headers. The switch and the spin button are therefore template children of
//! those two, where the other four rows are the editing widget themselves. Both hide that
//! behind `set_active`/`set_value`, so the factory reads the same either way.
//!
//! # A handler is a slot, not a signal
//!
//! Every `#[template_callback]` below does one thing: fire the closure the factory
//! installed, with the row as its argument. The row does not know what the closure does,
//! and the closure reads the value straight off the row it is handed — which is what lets
//! `options.rs` keep `commit`, `clear` and the `updating` guard in one place instead of six
//! widget files. A `glib` signal would need a `glib::Variant` shape for a payload that has
//! none, and would buy nothing: both ends are in this crate, in this process.

use std::cell::RefCell;
use std::rc::Rc;

mod choice;
mod flag;
mod folder;
mod number;
mod read_only;
mod text;

pub use choice::OptionRowChoice;
pub use flag::OptionRowFlag;
pub use folder::OptionRowFolder;
pub use number::OptionRowNumber;
pub use read_only::OptionRowReadOnly;
pub use text::OptionRowText;

/// Where a row keeps the closure the factory gave it for one of its handlers.
///
/// `Rc` rather than `Box` so [`fire`] can clone the closure out of the cell before calling
/// it: a handler writes the store, the store renders back into this same row, and a
/// `RefCell` borrow still held across that would be a panic rather than a redraw.
type Slot<T> = RefCell<Option<Rc<dyn Fn(&T)>>>;

/// Runs whatever the factory hung on this slot. A slot nothing was installed on does
/// nothing — a row built and not yet wired is not a bug, it is a row mid-render.
fn fire<T>(slot: &Slot<T>, row: &T) {
    let handler = slot.borrow().clone();
    if let Some(handler) = handler {
        handler(row);
    }
}

/// Replaces whatever was in the slot, so re-wiring a row cannot leave two handlers on it.
fn install<T>(slot: &Slot<T>, handler: impl Fn(&T) + 'static) {
    *slot.borrow_mut() = Some(Rc::new(handler));
}
