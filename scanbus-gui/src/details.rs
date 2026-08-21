//! The right-hand pane: what one scanner is, and where its keys are configured — the
//! composite-template subclass `details-pane.blp` declares.
//!
//! The layout is `docs/design/main.png` — three sections separated by rules. Identity at
//! the top (icon, name, the one-line summary the list row also shows), the facts in the
//! middle as [`DetailsFactRow`]s, and the full-width **Configure buttons** row at the
//! bottom. The pane is a fixed set of labels that [`DetailsPane::render`] fills from the
//! store rather than widgets built per scanner: the selection changes far more often than
//! the shape of the pane does, which is also why the whole shape is now in the `.blp` and
//! what is left here is three functions of a `Status` and two methods that write labels.

use gtk::{CompositeTemplate, TemplateChild, glib};
use gtk4 as gtk;
use libadwaita::prelude::*;
use libadwaita::subclass::prelude::*;
use scanbus_core::Status;

use crate::details_fact_row::DetailsFactRow;
use crate::scanners::{
    connection_value, default_profile_label, humanize_backend, status_summary, status_value,
};
use crate::store::ScannerEntry;

/// The status word's colour, and the only place a `Status` becomes one.
///
/// `Offline` is deliberately uncoloured: the design gives green to *Online* and leaves
/// the absence of a scanner as plain text, so the pane reads as one accent rather than a
/// row of traffic lights.
///
/// Stays in Rust rather than moving to the `.blp` with the rest of the shape: a class
/// applied *because of a property value* is content, and §3's "online is the only green"
/// is a unit test over this function.
fn status_css_class(status: Status) -> Option<&'static str> {
    match status {
        Status::Online => Some("success"),
        Status::Busy => Some("warning"),
        Status::Error => Some("error"),
        Status::Offline => None,
    }
}

/// The identity icon: what kind of thing this is, not which backend drives it.
///
/// A phone is the one scanner the user holds, so it gets `phone-symbolic`; everything
/// else — Brother, HPLIP, and any backend added later — is a machine on the network.
fn identity_icon_name(backend: &str) -> &'static str {
    if backend.to_ascii_lowercase().contains("mobile") {
        "phone-symbolic"
    } else {
        "printer-symbolic"
    }
}

mod imp {
    use std::sync::OnceLock;

    use glib::subclass::Signal;

    use super::*;

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/org/scanbus/Gui/ui/details-pane.ui")]
    pub struct DetailsPane {
        #[template_child]
        pub icon: TemplateChild<gtk::Image>,
        #[template_child]
        pub name: TemplateChild<gtk::Label>,
        /// The status word of the summary line — the half that carries the colour.
        #[template_child]
        pub summary_status: TemplateChild<gtk::Label>,
        /// The rest of the summary line, which never does.
        #[template_child]
        pub summary_rest: TemplateChild<gtk::Label>,

        /// The five facts. Each row's icon and title are set by `details-pane.blp`; what
        /// a render touches is the `value` label reached through it.
        #[template_child]
        pub status_row: TemplateChild<DetailsFactRow>,
        #[template_child]
        pub connection_row: TemplateChild<DetailsFactRow>,
        #[template_child]
        pub address_row: TemplateChild<DetailsFactRow>,
        #[template_child]
        pub backend_row: TemplateChild<DetailsFactRow>,
        #[template_child]
        pub default_profile_row: TemplateChild<DetailsFactRow>,

        /// The bottom section, hidden whole for a scanner that is not paired yet: both
        /// actions in it are pairing-only, and a lone rule over empty space is not a
        /// section. Rule and box are shown together, so both are held.
        #[template_child]
        pub actions_separator: TemplateChild<gtk::Separator>,
        #[template_child]
        pub actions: TemplateChild<gtk::Box>,
        #[template_child]
        pub configure_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub unpair: TemplateChild<gtk::Button>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DetailsPane {
        /// Must match `template $DetailsPane` in `details-pane.blp`.
        const NAME: &'static str = "DetailsPane";
        type Type = super::DetailsPane;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            // `details-pane.blp` names `$DetailsFactRow` five times and GtkBuilder
            // resolves that to a GType *by name*, so the class has to be registered
            // before this template is instantiated. Nothing else in the crate mentions
            // the type, so nothing else would register it.
            DetailsFactRow::ensure_type();

            klass.bind_template();
            // Instance callbacks, as in `window.rs`: the `#[gtk::template_callbacks]`
            // block is on the wrapper type.
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for DetailsPane {
        /// The two things the pane asks its owner for, as real signals rather than the
        /// `RefCell<Option<Box<dyn Fn>>>` slots this file used to hold.
        ///
        /// Neither can be a `#[template_callback]` on the pane alone: switching
        /// `detail_stack` and raising the confirmation are both `ScannersPane`'s to do,
        /// and it is the pane that knows whether the selected scanner is still paired.
        /// A signal is how a template subclass says "something happened here" without
        /// knowing who is listening.
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    Signal::builder("configure-requested").build(),
                    Signal::builder("unpair-requested").build(),
                ]
            })
        }
    }
    impl WidgetImpl for DetailsPane {}
    impl BoxImpl for DetailsPane {}
}

glib::wrapper! {
    /// The detail pane, built from `details-pane.blp` and written from the store.
    pub struct DetailsPane(ObjectSubclass<imp::DetailsPane>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl Default for DetailsPane {
    fn default() -> Self {
        Self::new()
    }
}

impl DetailsPane {
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// *Configure buttons* was activated. The pane does not switch the stack itself —
    /// see [`imp::DetailsPane::signals`].
    pub fn connect_configure_requested<F>(&self, callback: F) -> glib::SignalHandlerId
    where
        F: Fn() + 'static,
    {
        self.connect_local("configure-requested", false, move |_| {
            callback();
            None
        })
    }

    /// *Unpair* was clicked. Nothing has been sent yet: the confirmation is the
    /// listener's to raise.
    pub fn connect_unpair_requested<F>(&self, callback: F) -> glib::SignalHandlerId
    where
        F: Fn() + 'static,
    {
        self.connect_local("unpair-requested", false, move |_| {
            callback();
            None
        })
    }

    /// Writes the pane from one store entry: labels, the two visibilities, and the status
    /// classes. Everything else about the pane is in `details-pane.blp`.
    pub fn render(&self, scanner: &ScannerEntry) {
        let imp = self.imp();
        let state = &scanner.state;

        imp.icon
            .set_icon_name(Some(identity_icon_name(&state.backend)));
        imp.name.set_label(&state.name);

        let (lead, rest) = status_summary(state.status, state.connected, state.paired);
        imp.summary_status.set_label(lead);
        imp.summary_rest.set_label(rest);
        // An unpaired scanner leads with "Discovered", which is not a status and takes no
        // colour even when the device itself is online.
        set_status_class(&imp.summary_status, state.paired.then_some(state.status));

        let status = imp.status_row.value();
        status.set_label(status_value(state.status));
        set_status_class(&status, Some(state.status));

        imp.connection_row
            .value()
            .set_label(connection_value(state.connected));
        imp.address_row.value().set_label(&state.address);
        imp.backend_row
            .value()
            .set_label(&humanize_backend(&state.backend));
        imp.default_profile_row
            .value()
            .set_label(&default_profile_label(state.default_profile));

        imp.actions.set_visible(state.paired);
        imp.actions_separator.set_visible(state.paired);
    }

    /// Empties the pane, so a deselection does not leave the last scanner on screen.
    pub fn clear(&self) {
        let imp = self.imp();

        imp.icon.set_icon_name(Some(identity_icon_name("")));
        imp.name.set_label("");
        imp.summary_status.set_label("");
        imp.summary_rest.set_label("");
        set_status_class(&imp.summary_status, None);

        let status = imp.status_row.value();
        status.set_label("");
        set_status_class(&status, None);

        imp.connection_row.value().set_label("");
        imp.address_row.value().set_label("");
        imp.backend_row.value().set_label("");
        imp.default_profile_row.value().set_label("");

        imp.actions.set_visible(false);
        imp.actions_separator.set_visible(false);
    }
}

/// The two handlers `details-pane.blp` names, in the order the template mentions them.
///
/// Both are declared `swapped` there, which is what puts the pane in `&self`; each is one
/// emission, because what to do about it is not this pane's decision.
#[gtk::template_callbacks]
impl DetailsPane {
    /// Wired on the list rather than on the row: `row-activated` is what a plain
    /// `GtkListBox` emits for a click, with no `AdwPreferencesGroup` in between to
    /// forward it to the row's own activation. The row it hands over is dropped — the
    /// list holds exactly one.
    #[template_callback]
    fn on_configure_activated(&self) {
        self.emit_by_name::<()>("configure-requested", &[]);
    }

    #[template_callback]
    fn on_unpair_clicked(&self) {
        self.emit_by_name::<()>("unpair-requested", &[]);
    }
}

/// Applies the one status colour a label may carry, removing whichever it had.
fn set_status_class(label: &gtk::Label, status: Option<Status>) {
    for class in ["success", "warning", "error"] {
        label.remove_css_class(class);
    }
    if let Some(class) = status.and_then(status_css_class) {
        label.add_css_class(class);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_phone_is_drawn_as_a_phone() {
        assert_eq!(identity_icon_name("mobile"), "phone-symbolic");
        assert_eq!(identity_icon_name("hplip"), "printer-symbolic");
        assert_eq!(identity_icon_name("brother-skey"), "printer-symbolic");
        assert_eq!(
            identity_icon_name("proprietary:brother"),
            "printer-symbolic"
        );
        assert_eq!(identity_icon_name(""), "printer-symbolic");
    }

    #[test]
    fn online_is_the_only_green() {
        assert_eq!(status_css_class(Status::Online), Some("success"));
        assert_eq!(status_css_class(Status::Offline), None);
    }
}

/// The detail pane's half of the one GTK test — see [`crate::gtk_tests`].
#[cfg(all(test, feature = "gtk-tests"))]
pub(crate) mod widget_checks {
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    use scanbus_client::ScannerState;
    use scanbus_core::{Capabilities, PairingState, ProfileKind, ScannerId};

    use super::*;

    fn scanner(backend: &str, status: Status, connected: bool, paired: bool) -> ScannerEntry {
        let id = ScannerId::from_backend("mock", "usb:001:002").unwrap();
        ScannerEntry {
            path: scanbus_core::path::scanner(&id),
            state: ScannerState {
                id,
                name: "Brother MFC-L2710DW".to_owned(),
                backend: backend.to_owned(),
                address: "192.168.1.23".to_owned(),
                capabilities: Capabilities::default(),
                supported_profiles: vec![ProfileKind::Document],
                paired,
                connected,
                status,
                default_profile: Some(ProfileKind::Document),
                pairing: PairingState::Done,
            },
            buttons: BTreeMap::new(),
            jobs: BTreeMap::new(),
        }
    }

    pub(crate) fn run() {
        let pane = DetailsPane::new();
        let imp = pane.imp();

        pane.render(&scanner("proprietary:brother", Status::Online, true, true));

        assert_eq!(imp.icon.icon_name().as_deref(), Some("printer-symbolic"));
        assert_eq!(imp.name.label(), "Brother MFC-L2710DW");
        assert_eq!(imp.summary_status.label(), "Online");
        assert_eq!(imp.summary_rest.label(), " • Connected");
        assert!(
            imp.summary_status.has_css_class("success"),
            "the design gives the online summary its one accent"
        );
        assert_eq!(imp.status_row.value().label(), "Online");
        assert_eq!(imp.connection_row.value().label(), "Connected");
        assert_eq!(imp.address_row.value().label(), "192.168.1.23");
        assert_eq!(imp.backend_row.value().label(), "Brother");
        assert_eq!(imp.default_profile_row.value().label(), "Document");

        // Paired is what the bottom section is for; nothing to configure otherwise.
        assert!(imp.configure_list.is_visible());
        assert!(imp.unpair.is_visible());

        // Both actions, driven through the template rather than by calling the handlers:
        // this is the check that `swapped` is still on the two `=>` in `details-pane.blp`
        // — without it the emitting widget would be `self` and the pane would have no way
        // to reach `emit_by_name`. The counters are separate because two signals that
        // both fire on either action would pass a single-flag check.
        let configure_asked = Rc::new(Cell::new(0u32));
        let unpair_asked = Rc::new(Cell::new(0u32));
        pane.connect_configure_requested({
            let asked = Rc::clone(&configure_asked);
            move || asked.set(asked.get() + 1)
        });
        pane.connect_unpair_requested({
            let asked = Rc::clone(&unpair_asked);
            move || asked.set(asked.get() + 1)
        });

        // Emitted on the list, which is both where a click lands and where the `.blp`
        // wires the handler; driving the row's own `activate` instead would be asserting
        // against Adw.ActionRow's forwarding rather than against this pane.
        let configure_row = imp
            .configure_list
            .row_at_index(0)
            .expect("details-pane.blp puts the Configure buttons row in this list");
        imp.configure_list
            .emit_by_name::<()>("row-activated", &[&configure_row]);
        assert_eq!(
            (configure_asked.get(), unpair_asked.get()),
            (1, 0),
            "activating the row should have asked the pane to configure, and only that"
        );

        // Unpair asks; it does not unpair. The confirmation is `UnpairDialog`, and
        // `ScannersPane` is what raises it.
        imp.unpair.emit_clicked();
        assert_eq!(
            (configure_asked.get(), unpair_asked.get()),
            (1, 1),
            "the Unpair button should have asked the pane to confirm"
        );

        // Offline drops the accent rather than turning it another colour.
        pane.render(&scanner("hplip", Status::Offline, false, true));
        assert_eq!(imp.summary_status.label(), "Offline");
        assert_eq!(imp.summary_rest.label(), "");
        assert!(!imp.summary_status.has_css_class("success"));
        assert!(!imp.status_row.value().has_css_class("success"));
        assert_eq!(imp.connection_row.value().label(), "Disconnected");
        assert_eq!(imp.backend_row.value().label(), "HPLIP");

        // A discovered scanner leads with a word that is not a status, so it takes no
        // colour even though the device is online, and offers neither action.
        pane.render(&scanner("mobile", Status::Online, false, false));
        assert_eq!(imp.icon.icon_name().as_deref(), Some("phone-symbolic"));
        assert_eq!(imp.summary_status.label(), "Discovered");
        assert_eq!(imp.summary_rest.label(), " • Not paired");
        assert!(!imp.summary_status.has_css_class("success"));
        assert!(
            imp.status_row.value().has_css_class("success"),
            "the device is online"
        );
        assert_eq!(imp.backend_row.value().label(), "Mobile");
        assert!(!imp.configure_list.is_visible());
        assert!(!imp.unpair.is_visible());

        // Deselection empties the pane instead of leaving the last scanner on screen.
        pane.clear();
        assert_eq!(imp.name.label(), "");
        assert_eq!(imp.summary_status.label(), "");
        assert_eq!(imp.status_row.value().label(), "");
        assert_eq!(imp.address_row.value().label(), "");
        assert!(!imp.configure_list.is_visible());
        assert!(!imp.unpair.is_visible());
    }
}
