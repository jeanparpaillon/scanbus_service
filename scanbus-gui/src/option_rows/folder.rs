//! The row a path gets — the subclass `option-row-folder.blp` declares.
//!
//! The only editable row with nothing to watch: the chooser is what commits, so there is
//! no change handler. The dialog itself is not here — it needs the value the row currently
//! shows and the map to write back, neither of which a widget knows, so
//! [`OptionRowFolder::connect_choose`] hands the click to the factory and the factory runs
//! the `gtk::FileDialog`.

use gtk::{CompositeTemplate, TemplateChild, glib};
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::subclass::prelude::*;

use super::{Slot, fire, install};

mod imp {
    use super::*;

    // No `Debug`: a `Slot` holds a boxed closure, which does not derive it.
    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/org/scanbus/Gui/ui/option-row-folder.ui")]
    pub struct OptionRowFolder {
        #[template_child]
        pub origin: TemplateChild<gtk::Label>,
        #[template_child]
        pub reset: TemplateChild<gtk::Button>,
        #[template_child]
        pub chooser: TemplateChild<gtk::Button>,

        pub reset_clicked: Slot<super::OptionRowFolder>,
        pub choose: Slot<super::OptionRowFolder>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for OptionRowFolder {
        /// Must match `template $OptionRowFolder` in `option-row-folder.blp`.
        const NAME: &'static str = "OptionRowFolder";
        type Type = super::OptionRowFolder;
        type ParentType = adw::ActionRow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for OptionRowFolder {}
    impl WidgetImpl for OptionRowFolder {}
    impl ListBoxRowImpl for OptionRowFolder {}
    impl PreferencesRowImpl for OptionRowFolder {}
    impl ActionRowImpl for OptionRowFolder {}
}

glib::wrapper! {
    /// A path, shown as its shortened self with a folder chooser beside it.
    pub struct OptionRowFolder(ObjectSubclass<imp::OptionRowFolder>)
        @extends adw::ActionRow, adw::PreferencesRow, gtk::ListBoxRow, gtk::Widget,
        @implements gtk::Accessible, gtk::Actionable, gtk::Buildable, gtk::ConstraintTarget;
}

impl OptionRowFolder {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn origin(&self) -> gtk::Label {
        self.imp().origin.get()
    }

    pub fn reset(&self) -> gtk::Button {
        self.imp().reset.get()
    }

    /// **Choose…**, which the factory also needs as the widget to root the dialog on.
    pub fn chooser(&self) -> gtk::Button {
        self.imp().chooser.get()
    }

    /// What **Choose…** does. Replaces any previous handler.
    pub fn connect_choose(&self, handler: impl Fn(&Self) + 'static) {
        install(&self.imp().choose, handler);
    }

    /// What **Reset** does. Replaces any previous handler.
    pub fn connect_reset(&self, handler: impl Fn(&Self) + 'static) {
        install(&self.imp().reset_clicked, handler);
    }
}

impl Default for OptionRowFolder {
    fn default() -> Self {
        Self::new()
    }
}

/// The two handlers `option-row-folder.blp` names, in the order the template mentions
/// them. Both are declared `swapped` there, which is what puts the row in `&self`: both
/// emitters are buttons, so without it a handler could not reach the row at all.
#[gtk::template_callbacks]
impl OptionRowFolder {
    #[template_callback]
    fn on_option_reset(&self) {
        fire(&self.imp().reset_clicked, self);
    }

    #[template_callback]
    fn on_choose_folder(&self) {
        fire(&self.imp().choose, self);
    }
}
