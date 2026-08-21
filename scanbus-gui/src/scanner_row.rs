//! One row of either scanner list — the composite-template subclass `scanner-row.blp`
//! declares.
//!
//! Its own file rather than a function in `scanners.rs`, because it is the first widget
//! the lists build *n* of: `ScannersPane` binds each `Gtk.FilterListModel` with a factory
//! that does nothing but `ScannerRow::new(item, …).upcast()`, and everything a row knows
//! about a scanner is reached through the `scanner` property set there.
//!
//! Rows update in place. §3 of the design doc says a `Status` change from `online` to
//! `busy` must not move the selection or collapse the detail pane, which holds only as
//! long as a store change repaints the row that already exists instead of building a new
//! one. Two things keep that true and both are here: the title and subtitle are
//! `bind_property` bindings on the `ScannerObject`, and everything else is written by
//! [`ScannerRow::render`] from a `notify` handler on the same object. Nothing calls
//! `ScannerRow::new` but the two factories.

use std::cell::{OnceCell, RefCell};
use std::rc::Rc;

use async_channel::Sender;
use gtk::{CompositeTemplate, TemplateChild, glib};
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use libadwaita::subclass::prelude::*;

use crate::bus::BusCommand;
use crate::scanners::{ScannerListModel, ScannerObject, humanize_backend};

mod imp {
    use super::*;

    // No `Debug`: `Rc<ScannerListModel>` does not derive it, for the reason `window.rs`
    // gives.
    #[derive(Default, CompositeTemplate, glib::Properties)]
    #[template(resource = "/org/scanbus/Gui/ui/scanner-row.ui")]
    #[properties(wrapper_type = super::ScannerRow)]
    pub struct ScannerRow {
        #[template_child]
        pub title: TemplateChild<gtk::Label>,
        #[template_child]
        pub subtitle: TemplateChild<gtk::Label>,
        #[template_child]
        pub pair_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub progress_box: TemplateChild<gtk::Box>,
        #[template_child]
        pub spinner: TemplateChild<gtk::Spinner>,
        #[template_child]
        pub progress_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub confirm_box: TemplateChild<gtk::Box>,
        #[template_child]
        pub code_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub failure_box: TemplateChild<gtk::Box>,
        #[template_child]
        pub failure_banner: TemplateChild<adw::Bin>,
        #[template_child]
        pub failure_message: TemplateChild<gtk::Label>,
        #[template_child]
        pub failure_details_label: TemplateChild<gtk::Label>,

        /// The model item this row shows.
        ///
        /// A property and not a plain cell, unlike the two handles below: it is the one
        /// piece of a row's state that is a `glib::Value`, and making it one is what lets
        /// the factory hand the object to `glib::Object::builder` rather than reaching
        /// into `imp` from outside. Nullable because a `GObject` property has to have a
        /// default, and there is no scanner before the factory names one.
        #[property(get, set, nullable)]
        pub scanner: RefCell<Option<ScannerObject>>,

        /// The two non-`Value` handles the callbacks need, as `OnceCell`s for the reason
        /// `window.rs` gives: neither is a `glib::Value`, and a `#[template_callback]`
        /// reaches them through `self.imp()` either way.
        pub commands: OnceCell<Sender<BusCommand>>,
        pub model: OnceCell<Rc<ScannerListModel>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ScannerRow {
        /// Must match `template $ScannerRow` in `scanner-row.blp`.
        const NAME: &'static str = "ScannerRow";
        type Type = super::ScannerRow;
        type ParentType = gtk::ListBoxRow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            // Instance callbacks, as in `window.rs`: the `#[gtk::template_callbacks]`
            // block is on the wrapper type.
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    #[glib::derived_properties]
    impl ObjectImpl for ScannerRow {}
    impl WidgetImpl for ScannerRow {}
    impl ListBoxRowImpl for ScannerRow {}
}

glib::wrapper! {
    /// A row of either scanner list, built from `scanner-row.blp`.
    pub struct ScannerRow(ObjectSubclass<imp::ScannerRow>)
        @extends gtk::ListBoxRow, gtk::Widget,
        @implements gtk::Accessible, gtk::Actionable, gtk::Buildable, gtk::ConstraintTarget;
}

impl ScannerRow {
    /// Builds one row for one model item.
    ///
    /// Called from the `bind_model` factories of `ScannersPane` and nowhere else: a row
    /// built anywhere else would be a second widget for a scanner that already has one,
    /// which is exactly the rebuild the in-place rule forbids.
    pub fn new(
        scanner: &ScannerObject,
        commands: Sender<BusCommand>,
        model: Rc<ScannerListModel>,
    ) -> Self {
        let row: Self = glib::Object::builder().property("scanner", scanner).build();

        // The object was built on the line above and this is its only constructor, so
        // neither `set` can fail; the asserts say that rather than dropping a handle on
        // the floor should it ever stop being true.
        let imp = row.imp();
        assert!(imp.commands.set(commands).is_ok(), "commands set twice");
        assert!(imp.model.set(model).is_ok(), "model set twice");

        // The two bindings that make a name or a status line change repaint the row it is
        // already in. They stay here rather than becoming `bind` expressions in the
        // `.blp` because the property above is set on the line before this one — a
        // binding declared in the template would be evaluated against a row with no
        // scanner and would leave an empty label with no error to notice.
        scanner
            .bind_property("name", &*imp.title, "label")
            .sync_create()
            .build();
        scanner
            .bind_property("status-line", &*imp.subtitle, "label")
            .sync_create()
            .build();

        // The widget name is the object path: it is what the pane's `row-selected`
        // handlers read back and hand to the store, so it is per item and not in the
        // `.blp`.
        row.set_widget_name(&scanner.path());

        // Everything the two bindings do not cover — the pair button's label, and which
        // of the three pairing sub-boxes is up. A weak handle because the `ScannerObject`
        // outlives the row: it is owned by the store's `ListStore`, and a strong `self`
        // here would keep every row that has ever been built alive for as long as its
        // scanner is known.
        let weak = row.downgrade();
        scanner.connect_notify_local(None, move |_, _| {
            if let Some(row) = weak.upgrade() {
                row.render();
            }
        });
        row.render();

        row
    }

    /// Writes the pairing half of the row from the `ScannerObject`.
    ///
    /// The three sub-boxes are hidden first and then one is shown, so a state the match
    /// below does not name — `idle`, `paired` — leaves the row with the button alone,
    /// which is what "not pairing" looks like.
    fn render(&self) {
        let imp = self.imp();
        let Some(scanner) = imp.scanner.borrow().clone() else {
            return;
        };

        let paired = scanner.paired();
        let pairing = scanner.pairing_state();
        let code = scanner.pairing_code();
        let error = scanner.pairing_error();

        imp.pair_button.set_visible(!paired);
        imp.pair_button
            .set_sensitive(!paired || pairing == "failed");

        imp.progress_box.set_visible(false);
        imp.confirm_box.set_visible(false);
        imp.failure_box.set_visible(false);
        imp.spinner.set_visible(false);
        imp.spinner.set_spinning(false);
        imp.failure_banner.set_visible(false);

        match pairing.as_str() {
            "pairing" => {
                imp.pair_button.set_label("Cancel");
                imp.progress_box.set_visible(true);
                imp.spinner.set_visible(true);
                imp.spinner.set_spinning(true);
                imp.progress_label.set_label("Pairing in progress");
            }
            "installing_backend" => {
                imp.pair_button.set_label("Cancel");
                imp.progress_box.set_visible(true);
                imp.spinner.set_visible(true);
                imp.spinner.set_spinning(true);
                imp.progress_label.set_label(&format!(
                    "Installing {} backend…",
                    humanize_backend(&scanner.backend())
                ));
            }
            "awaiting_confirmation" => {
                imp.pair_button.set_label("Cancel");
                imp.confirm_box.set_visible(true);
                imp.code_label.set_label(&code);
            }
            "failed" => {
                imp.pair_button.set_label("Try again");
                imp.failure_box.set_visible(true);
                imp.failure_banner.set_visible(true);
                imp.failure_message.set_label("Pairing failed");
                imp.failure_details_label.set_label(&error);
            }
            _ => {
                imp.pair_button.set_label("Pair");
            }
        }
    }
}

/// The two handlers `scanner-row.blp` names, in the order the template mentions them.
///
/// Both are declared `swapped` there, which is what puts the row in `&self`.
#[gtk::template_callbacks]
impl ScannerRow {
    /// Activating a row selects its scanner. The pane's `row-selected` handlers do the
    /// same thing for a selection made with the pointer; this is the keyboard's half.
    #[template_callback]
    fn on_scanner_row_activate(&self) {
        let imp = self.imp();
        let Some(scanner) = imp.scanner.borrow().clone() else {
            return;
        };
        imp.model
            .get()
            .expect("model set in `new`")
            .set_selected_path(Some(scanner.path()));
    }

    /// Pair, or cancel while pairing.
    ///
    /// The label says which — "Pair", then "Cancel" while pairing, "Try again" after a
    /// failure — but the decision is read off the scanner's `PairingState` and not off
    /// the label, so a repaint that has not happened yet cannot send the wrong command.
    #[template_callback]
    fn on_pair_clicked(&self) {
        let imp = self.imp();
        let Some(scanner) = imp.scanner.borrow().clone() else {
            return;
        };
        let commands = imp.commands.get().expect("commands set in `new`");

        let path = scanner.path();
        match scanner.pairing_state().as_str() {
            "pairing" | "installing_backend" | "awaiting_confirmation" => {
                let _ = commands.try_send(BusCommand::CancelPairing { path });
            }
            _ => {
                let _ = commands.try_send(BusCommand::Pair { path });
            }
        }
    }
}

/// The row's half of the one GTK test — see [`crate::gtk_tests`].
///
/// Called from `scanners.rs`'s checks rather than listed in `gtk_tests.rs`, and taking a
/// row rather than building one: `ScannerRow::new` has two intended callers, both
/// `bind_model` factories, and a check that called it a third time would be asserting
/// against a widget no list holds — the rebuild the in-place rule forbids, written into
/// the test suite. So the pane's checks take a row out of a `Gtk.ListBox` and hand it
/// here, where `imp` is visible.
///
/// The states below are driven by setting properties on the `ScannerObject`, which is
/// exactly what `ScannerListModel::update_from_entry` does when a `PropertiesChanged`
/// arrives; going through the store as well would only be asserting that `scanners.rs`
/// still calls it.
#[cfg(all(test, feature = "gtk-tests"))]
pub(crate) mod widget_checks {
    use async_channel::Receiver;

    use super::*;

    /// Asserts against a discovered row — one that offers *Pair* — and leaves it back in
    /// the state it was found in, apart from the selection its last check makes.
    pub(crate) fn run(row: &ScannerRow, sent: &Receiver<BusCommand>) {
        let imp = row.imp();
        let scanner = row
            .scanner()
            .expect("the factory sets the scanner property");
        let path = scanner.path();

        // `init_template` filled the twelve children above from `scanner-row.blp`; these
        // are the two that are written by a binding rather than by `render`. The
        // template's own labels are empty, so a title that reads as the scanner's name is
        // the `sync_create` and not the `.blp`.
        assert_eq!(imp.title.label(), scanner.name());
        assert_eq!(imp.subtitle.label(), scanner.status_line());
        assert_eq!(
            row.widget_name(),
            path,
            "the pane's `row-selected` handlers read the object path back off the widget"
        );

        // Not pairing: the button, and none of the three sub-boxes.
        assert!(imp.pair_button.get_visible());
        assert!(imp.pair_button.get_sensitive());
        assert_eq!(imp.pair_button.label().unwrap_or_default(), "Pair");
        assert!(!imp.progress_box.get_visible());
        assert!(!imp.confirm_box.get_visible());
        assert!(!imp.failure_box.get_visible());

        // Clicked through the template, which is the check that `swapped` is still on the
        // handler in the `.blp`: without it `self` would be the button and neither the
        // bus channel nor the scanner would be reachable.
        imp.pair_button.emit_clicked();
        assert!(
            matches!(sent.try_recv(), Ok(BusCommand::Pair { path: asked }) if asked == path),
            "the Pair button should have asked the bus thread to pair this scanner"
        );

        // Pairing: the same row, repainted. A rebuilt row is what would lose the
        // selection at the worst moment, so every state below is asserted on the widget
        // this function was handed.
        scanner.set_property("pairing-state", "pairing");
        assert_eq!(imp.pair_button.label().unwrap_or_default(), "Cancel");
        assert!(imp.progress_box.get_visible());
        assert!(imp.spinner.get_visible() && imp.spinner.is_spinning());
        assert_eq!(imp.progress_label.label(), "Pairing in progress");

        // The label says Cancel and so does the command, but the decision is read off the
        // `PairingState` — this is what says the two have not come apart.
        imp.pair_button.emit_clicked();
        assert!(
            matches!(sent.try_recv(), Ok(BusCommand::CancelPairing { path: asked }) if asked == path),
            "Cancel should have asked the bus thread to cancel the pairing"
        );

        // Still pairing, and the one state that names the backend.
        scanner.set_property("pairing-state", "installing_backend");
        assert!(imp.spinner.is_spinning());
        assert_eq!(
            imp.progress_label.label(),
            format!(
                "Installing {} backend…",
                humanize_backend(&scanner.backend())
            )
        );

        // Awaiting confirmation: the code, and the progress box gone rather than left
        // under it.
        scanner.set_property("pairing-code", "482913");
        scanner.set_property("pairing-state", "awaiting_confirmation");
        assert!(imp.confirm_box.get_visible());
        assert_eq!(imp.code_label.label(), "482913");
        assert!(!imp.progress_box.get_visible());
        assert!(!imp.spinner.is_spinning());

        // Failed: the retry affordance §3 asks for, and the message the daemon gave.
        scanner.set_property("pairing-error", "backend install failed");
        scanner.set_property("pairing-state", "failed");
        assert!(imp.failure_box.get_visible());
        assert!(imp.failure_banner.get_visible());
        assert_eq!(imp.failure_message.label(), "Pairing failed");
        assert_eq!(imp.failure_details_label.label(), "backend install failed");
        assert_eq!(imp.pair_button.label().unwrap_or_default(), "Try again");
        assert!(
            imp.pair_button.get_sensitive(),
            "a failed pairing has to be retryable"
        );
        assert!(!imp.confirm_box.get_visible());

        // Try again is a `Pair`, not a state of its own.
        imp.pair_button.emit_clicked();
        assert!(
            matches!(sent.try_recv(), Ok(BusCommand::Pair { path: asked }) if asked == path),
            "Try again should have asked the bus thread to pair again"
        );

        // Back to where the row started: a state the match does not name leaves the
        // button alone, which is what "not pairing" looks like.
        scanner.set_property("pairing-state", "none");
        assert_eq!(imp.pair_button.label().unwrap_or_default(), "Pair");
        assert!(!imp.progress_box.get_visible());
        assert!(!imp.confirm_box.get_visible());
        assert!(!imp.failure_box.get_visible());
        assert!(!imp.spinner.is_spinning());

        // The keyboard's half of selection: the pane's `row-selected` handlers cover the
        // pointer, and this handler is why a row reached with the arrow keys and Enter
        // reaches the same store.
        row.emit_activate();
        assert_eq!(
            imp.model
                .get()
                .expect("model set in `new`")
                .selected_path()
                .as_deref(),
            Some(path.as_str()),
            "activating a row should select its scanner"
        );
    }
}
