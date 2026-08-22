//! The row a string with no closed set gets — the subclass `option-row-text.blp`
//! declares.
//!
//! The one row that does not commit on change: `AdwEntryRow`'s own apply button is the
//! commit, because every keystroke would otherwise be a write to the daemon. So the
//! handler the factory installs with [`OptionRowText::connect_applied`] fires on *apply*,
//! not on every edit — the name says which.
//!
//! The text itself is `AdwEntryRow`'s, reached through `gtk::Editable`, so there is no
//! accessor for it here.

use gtk::{CompositeTemplate, TemplateChild, glib};
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::subclass::prelude::*;

use super::{Slot, fire, install};

mod imp {
    use super::*;

    // No `Debug`: a `Slot` holds a boxed closure, which does not derive it.
    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/org/scanbus/Gui/ui/option-row-text.ui")]
    pub struct OptionRowText {
        #[template_child]
        pub origin: TemplateChild<gtk::Label>,
        #[template_child]
        pub reset: TemplateChild<gtk::Button>,

        pub applied: Slot<super::OptionRowText>,
        pub reset_clicked: Slot<super::OptionRowText>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for OptionRowText {
        /// Must match `template $OptionRowText` in `option-row-text.blp`.
        const NAME: &'static str = "OptionRowText";
        type Type = super::OptionRowText;
        type ParentType = adw::EntryRow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for OptionRowText {}
    impl WidgetImpl for OptionRowText {}
    impl ListBoxRowImpl for OptionRowText {}
    impl PreferencesRowImpl for OptionRowText {}
    impl EntryRowImpl for OptionRowText {}
}

glib::wrapper! {
    /// A string with no closed set, drawn as an entry with an apply button.
    pub struct OptionRowText(ObjectSubclass<imp::OptionRowText>)
        @extends adw::EntryRow, adw::PreferencesRow, gtk::ListBoxRow, gtk::Widget,
        @implements gtk::Accessible, gtk::Actionable, gtk::Buildable, gtk::ConstraintTarget,
                    gtk::Editable;
}

impl OptionRowText {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn origin(&self) -> gtk::Label {
        self.imp().origin.get()
    }

    pub fn reset(&self) -> gtk::Button {
        self.imp().reset.get()
    }

    /// What the apply button does. Replaces any previous handler.
    pub fn connect_applied(&self, handler: impl Fn(&Self) + 'static) {
        install(&self.imp().applied, handler);
    }

    /// What **Reset** does. Replaces any previous handler.
    pub fn connect_reset(&self, handler: impl Fn(&Self) + 'static) {
        install(&self.imp().reset_clicked, handler);
    }
}

impl Default for OptionRowText {
    fn default() -> Self {
        Self::new()
    }
}

/// The two handlers `option-row-text.blp` names, in the order the template mentions them.
/// Both are declared `swapped` there, which is what puts the row in `&self`.
#[gtk::template_callbacks]
impl OptionRowText {
    #[template_callback]
    fn on_text_applied(&self) {
        fire(&self.imp().applied, self);
    }

    #[template_callback]
    fn on_option_reset(&self) {
        fire(&self.imp().reset_clicked, self);
    }
}
