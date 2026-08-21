//! One fact of the detail pane's middle section — the composite-template subclass
//! `details-fact-row.blp` declares.
//!
//! Its own file for the same reason [`crate::scanner_row`] has one: it is a shape the
//! pane holds five of, so the three things that differ between them — icon, title, and
//! whether the value can be selected — are *properties* of this class rather than ids in
//! `details-pane.blp`. That is what lets the pane keep one template child per row
//! instead of three, and what makes a sixth fact one four-line block in the `.blp`.
//!
//! Nothing in Rust ever calls a constructor here: the five instances are built by
//! GtkBuilder from `details-pane.blp`, which is why `imp::DetailsPane::class_init` has to
//! `ensure_type` this class before binding its own template.

use std::cell::{Cell, RefCell};

use gtk::{CompositeTemplate, TemplateChild, glib};
use gtk4 as gtk;
use libadwaita::prelude::*;
use libadwaita::subclass::prelude::*;

mod imp {
    use super::*;

    #[derive(Default, CompositeTemplate, glib::Properties)]
    #[template(resource = "/org/scanbus/Gui/ui/details-fact-row.ui")]
    #[properties(wrapper_type = super::DetailsFactRow)]
    pub struct DetailsFactRow {
        /// The value half — the only id in the `.blp`, because it is the only half a
        /// render writes.
        #[template_child]
        pub value: TemplateChild<gtk::Label>,

        /// The three the template binds into its children. They are properties and not
        /// template children so that `details-pane.blp` can declare them per instance;
        /// each is read back by exactly one `bind` expression in `details-fact-row.blp`.
        #[property(get, set)]
        pub icon_name: RefCell<String>,
        #[property(get, set)]
        pub title: RefCell<String>,
        #[property(get, set)]
        pub value_selectable: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DetailsFactRow {
        /// Must match `template $DetailsFactRow` in `details-fact-row.blp`.
        const NAME: &'static str = "DetailsFactRow";
        type Type = super::DetailsFactRow;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    #[glib::derived_properties]
    impl ObjectImpl for DetailsFactRow {}
    impl WidgetImpl for DetailsFactRow {}
    impl BoxImpl for DetailsFactRow {}
}

glib::wrapper! {
    /// Icon, title, and the value pushed to the right margin.
    pub struct DetailsFactRow(ObjectSubclass<imp::DetailsFactRow>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl DetailsFactRow {
    /// The label a render writes, and the status class a status row carries.
    ///
    /// Returned rather than exposed as a `label` property: the pane also adds and removes
    /// a css class on it, which no property would carry.
    pub fn value(&self) -> gtk::Label {
        self.imp().value.get()
    }
}
