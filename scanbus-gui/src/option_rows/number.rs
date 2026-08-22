//! The row a bounded integer gets — the subclass `option-row-number.blp` declares.
//!
//! An `AdwActionRow` with a `GtkSpinButton` in its suffix, and not an `AdwSpinRow`, which
//! is final and cannot be derived from; [`super`] and the `.blp` header say why at length.
//! The two accessors below are what keeps that a detail of this file: the factory sets a
//! value and reads one back, exactly as it did on the `AdwSpinRow`.
//!
//! The adjustment is where the schema's `min` and `max` land, and it is what refuses an
//! out-of-range value — the template declares one at zero for the factory to write per
//! render.

use gtk::{CompositeTemplate, TemplateChild, glib};
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::subclass::prelude::*;

use super::{Slot, fire, install};

mod imp {
    use super::*;

    // No `Debug`: a `Slot` holds a boxed closure, which does not derive it.
    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/org/scanbus/Gui/ui/option-row-number.ui")]
    pub struct OptionRowNumber {
        #[template_child]
        pub origin: TemplateChild<gtk::Label>,
        #[template_child]
        pub reset: TemplateChild<gtk::Button>,
        /// The editing widget, which on the other four editable rows is the row itself.
        #[template_child]
        pub spin: TemplateChild<gtk::SpinButton>,

        pub changed: Slot<super::OptionRowNumber>,
        pub reset_clicked: Slot<super::OptionRowNumber>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for OptionRowNumber {
        /// Must match `template $OptionRowNumber` in `option-row-number.blp`.
        const NAME: &'static str = "OptionRowNumber";
        type Type = super::OptionRowNumber;
        type ParentType = adw::ActionRow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for OptionRowNumber {}
    impl WidgetImpl for OptionRowNumber {}
    impl ListBoxRowImpl for OptionRowNumber {}
    impl PreferencesRowImpl for OptionRowNumber {}
    impl ActionRowImpl for OptionRowNumber {}
}

glib::wrapper! {
    /// An integer within its published bounds.
    pub struct OptionRowNumber(ObjectSubclass<imp::OptionRowNumber>)
        @extends adw::ActionRow, adw::PreferencesRow, gtk::ListBoxRow, gtk::Widget,
        @implements gtk::Accessible, gtk::Actionable, gtk::Buildable, gtk::ConstraintTarget;
}

impl OptionRowNumber {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn origin(&self) -> gtk::Label {
        self.imp().origin.get()
    }

    pub fn reset(&self) -> gtk::Button {
        self.imp().reset.get()
    }

    /// The adjustment the schema's bounds are written to.
    pub fn adjustment(&self) -> gtk::Adjustment {
        self.imp().spin.adjustment()
    }

    /// What the row settled on — which is not always what was asked for, because the
    /// adjustment's bounds clamp.
    pub fn value(&self) -> f64 {
        self.imp().spin.value()
    }

    pub fn set_value(&self, value: f64) {
        self.imp().spin.set_value(value);
    }

    /// What a new value does. Replaces any previous handler.
    pub fn connect_changed(&self, handler: impl Fn(&Self) + 'static) {
        install(&self.imp().changed, handler);
    }

    /// What **Reset** does. Replaces any previous handler.
    pub fn connect_reset(&self, handler: impl Fn(&Self) + 'static) {
        install(&self.imp().reset_clicked, handler);
    }
}

impl Default for OptionRowNumber {
    fn default() -> Self {
        Self::new()
    }
}

/// The two handlers `option-row-number.blp` names, in the order the template mentions
/// them. Both are declared `swapped` there, which is what puts the row in `&self` — the
/// spin button is a child here, so without it neither could reach the row.
#[gtk::template_callbacks]
impl OptionRowNumber {
    #[template_callback]
    fn on_number_changed(&self) {
        fire(&self.imp().changed, self);
    }

    #[template_callback]
    fn on_option_reset(&self) {
        fire(&self.imp().reset_clicked, self);
    }
}
