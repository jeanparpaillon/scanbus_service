//! The row a type this version cannot draw gets — the subclass `option-row-read-only.blp`
//! declares, and the one an undeclared key gets too.
//!
//! No Reset and no handlers: nothing here can be written, so nothing can be cleared. It is
//! the only one of the six with a slot-free template, which is why it has no
//! `connect_` anything — an option with no editor must not offer to change the value
//! either.

use gtk::{CompositeTemplate, TemplateChild, glib};
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::subclass::prelude::*;

mod imp {
    use super::*;

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(resource = "/org/scanbus/Gui/ui/option-row-read-only.ui")]
    pub struct OptionRowReadOnly {
        #[template_child]
        pub origin: TemplateChild<gtk::Label>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for OptionRowReadOnly {
        /// Must match `template $OptionRowReadOnly` in `option-row-read-only.blp`.
        const NAME: &'static str = "OptionRowReadOnly";
        type Type = super::OptionRowReadOnly;
        type ParentType = adw::ActionRow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for OptionRowReadOnly {}
    impl WidgetImpl for OptionRowReadOnly {}
    impl ListBoxRowImpl for OptionRowReadOnly {}
    impl PreferencesRowImpl for OptionRowReadOnly {}
    impl ActionRowImpl for OptionRowReadOnly {}
}

glib::wrapper! {
    /// An unknown type, or a key the schema no longer declares: shown, never hidden.
    pub struct OptionRowReadOnly(ObjectSubclass<imp::OptionRowReadOnly>)
        @extends adw::ActionRow, adw::PreferencesRow, gtk::ListBoxRow, gtk::Widget,
        @implements gtk::Accessible, gtk::Actionable, gtk::Buildable, gtk::ConstraintTarget;
}

impl OptionRowReadOnly {
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// The pill. An undeclared key had none before this port — it was built and never
    /// parented — so this is the row where both cases finally say the same thing.
    pub fn origin(&self) -> gtk::Label {
        self.imp().origin.get()
    }
}

impl Default for OptionRowReadOnly {
    fn default() -> Self {
        Self::new()
    }
}
