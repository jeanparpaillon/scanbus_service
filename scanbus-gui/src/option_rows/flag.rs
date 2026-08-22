//! The row a boolean gets — the subclass `option-row-flag.blp` declares.
//!
//! An `AdwActionRow` with a `GtkSwitch` in its suffix, and not an `AdwSwitchRow`, which is
//! final and cannot be derived from; [`super`] and the `.blp` header say why at length.
//! The two accessors below are what keeps that a detail of this file: the factory sets the
//! flag and reads it back, exactly as it did on the `AdwSwitchRow`.

use gtk::{CompositeTemplate, TemplateChild, glib};
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::subclass::prelude::*;

use super::{Slot, fire, install};

mod imp {
    use super::*;

    // No `Debug`: a `Slot` holds a boxed closure, which does not derive it.
    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/org/scanbus/Gui/ui/option-row-flag.ui")]
    pub struct OptionRowFlag {
        #[template_child]
        pub origin: TemplateChild<gtk::Label>,
        #[template_child]
        pub reset: TemplateChild<gtk::Button>,
        /// The editing widget, which on the other four editable rows is the row itself.
        /// The template also makes it the row's `activatable-widget`, so a click anywhere
        /// on the row toggles it — which is the behaviour `AdwSwitchRow` would have given.
        #[template_child]
        pub toggle: TemplateChild<gtk::Switch>,

        pub changed: Slot<super::OptionRowFlag>,
        pub reset_clicked: Slot<super::OptionRowFlag>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for OptionRowFlag {
        /// Must match `template $OptionRowFlag` in `option-row-flag.blp`.
        const NAME: &'static str = "OptionRowFlag";
        type Type = super::OptionRowFlag;
        type ParentType = adw::ActionRow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for OptionRowFlag {}
    impl WidgetImpl for OptionRowFlag {}
    impl ListBoxRowImpl for OptionRowFlag {}
    impl PreferencesRowImpl for OptionRowFlag {}
    impl ActionRowImpl for OptionRowFlag {}
}

glib::wrapper! {
    /// A boolean, drawn as a switch.
    pub struct OptionRowFlag(ObjectSubclass<imp::OptionRowFlag>)
        @extends adw::ActionRow, adw::PreferencesRow, gtk::ListBoxRow, gtk::Widget,
        @implements gtk::Accessible, gtk::Actionable, gtk::Buildable, gtk::ConstraintTarget;
}

impl OptionRowFlag {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn origin(&self) -> gtk::Label {
        self.imp().origin.get()
    }

    pub fn reset(&self) -> gtk::Button {
        self.imp().reset.get()
    }

    pub fn is_active(&self) -> bool {
        self.imp().toggle.is_active()
    }

    pub fn set_active(&self, active: bool) {
        self.imp().toggle.set_active(active);
    }

    /// What a flip of the switch does. Replaces any previous handler.
    pub fn connect_changed(&self, handler: impl Fn(&Self) + 'static) {
        install(&self.imp().changed, handler);
    }

    /// What **Reset** does. Replaces any previous handler.
    pub fn connect_reset(&self, handler: impl Fn(&Self) + 'static) {
        install(&self.imp().reset_clicked, handler);
    }
}

impl Default for OptionRowFlag {
    fn default() -> Self {
        Self::new()
    }
}

/// The two handlers `option-row-flag.blp` names, in the order the template mentions them.
/// Both are declared `swapped` there, which is what puts the row in `&self` — the switch
/// is a child here, so without it neither could reach the row.
#[gtk::template_callbacks]
impl OptionRowFlag {
    #[template_callback]
    fn on_flag_toggled(&self) {
        fire(&self.imp().changed, self);
    }

    #[template_callback]
    fn on_option_reset(&self) {
        fire(&self.imp().reset_clicked, self);
    }
}
