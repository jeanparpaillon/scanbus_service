//! The row a closed set of values gets — the subclass `option-row-choice.blp` declares.
//!
//! The dropdown's items are not shape and are not here: the template declares an empty
//! `Gtk.StringList` for the row to hang a model on, and the factory fills it per render
//! from the schema's `values` plus whatever is actually set. See [`super`].

use gtk::{CompositeTemplate, TemplateChild, glib};
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use libadwaita::subclass::prelude::*;

use super::{Slot, fire, install};

mod imp {
    use super::*;

    // No `Debug`: a `Slot` holds a boxed closure, which does not derive it.
    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/org/scanbus/Gui/ui/option-row-choice.ui")]
    pub struct OptionRowChoice {
        #[template_child]
        pub origin: TemplateChild<gtk::Label>,
        #[template_child]
        pub reset: TemplateChild<gtk::Button>,

        /// What a new selection and a **Reset** click do. Both are filled by the factory
        /// in `options.rs`, which is the only thing that knows this row has a key.
        pub changed: Slot<super::OptionRowChoice>,
        pub reset_clicked: Slot<super::OptionRowChoice>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for OptionRowChoice {
        /// Must match `template $OptionRowChoice` in `option-row-choice.blp`.
        const NAME: &'static str = "OptionRowChoice";
        type Type = super::OptionRowChoice;
        type ParentType = adw::ComboRow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            // Instance callbacks, as in `scanner_row.rs`: the `#[gtk::template_callbacks]`
            // block is on the wrapper type.
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for OptionRowChoice {}
    impl WidgetImpl for OptionRowChoice {}
    impl ListBoxRowImpl for OptionRowChoice {}
    impl PreferencesRowImpl for OptionRowChoice {}
    impl ActionRowImpl for OptionRowChoice {}
    impl ComboRowImpl for OptionRowChoice {}
}

glib::wrapper! {
    /// A closed set of strings, drawn as a dropdown.
    pub struct OptionRowChoice(ObjectSubclass<imp::OptionRowChoice>)
        @extends adw::ComboRow, adw::ActionRow, adw::PreferencesRow, gtk::ListBoxRow,
                 gtk::Widget,
        @implements gtk::Accessible, gtk::Actionable, gtk::Buildable, gtk::ConstraintTarget;
}

impl OptionRowChoice {
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// The "Inherited"/"Override" pill. Returned rather than exposed as a label property:
    /// the factory also decides whether it is shown at all, which no property carries.
    pub fn origin(&self) -> gtk::Label {
        self.imp().origin.get()
    }

    /// **Reset**. Its tooltip names the scope, so the factory writes it per row.
    pub fn reset(&self) -> gtk::Button {
        self.imp().reset.get()
    }

    /// The dropdown's items — the `Gtk.StringList` the template declared empty, which a
    /// render splices the schema's `values` into. The mirror of
    /// [`OptionRowNumber::adjustment`](super::OptionRowNumber::adjustment): the template
    /// declares the object, the factory writes what is in it.
    pub fn items(&self) -> gtk::StringList {
        self.model()
            .and_downcast()
            .expect("option-row-choice.blp declares the model as a Gtk.StringList")
    }

    /// What a new selection does. Replaces any previous handler.
    pub fn connect_changed(&self, handler: impl Fn(&Self) + 'static) {
        install(&self.imp().changed, handler);
    }

    /// What **Reset** does. Replaces any previous handler.
    pub fn connect_reset(&self, handler: impl Fn(&Self) + 'static) {
        install(&self.imp().reset_clicked, handler);
    }
}

impl Default for OptionRowChoice {
    fn default() -> Self {
        Self::new()
    }
}

/// The two handlers `option-row-choice.blp` names, in the order the template mentions
/// them. Both are declared `swapped` there, which is what puts the row in `&self`.
#[gtk::template_callbacks]
impl OptionRowChoice {
    #[template_callback]
    fn on_choice_changed(&self) {
        fire(&self.imp().changed, self);
    }

    #[template_callback]
    fn on_option_reset(&self) {
        fire(&self.imp().reset_clicked, self);
    }
}
