//! The right-hand pane: what one scanner is, and where its keys are configured.
//!
//! The layout is `docs/design/main.png` — three sections separated by rules. Identity at
//! the top (icon, name, the one-line summary the list row also shows), the facts in the
//! middle as icon / label / value, and the full-width **Configure buttons** row at the
//! bottom. Every fact is a row of the same shape, so the pane is a fixed set of labels
//! that [`DetailsPane::render`] fills from the store rather than widgets built per
//! scanner: the selection changes far more often than the shape of the pane does.

use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use scanbus_core::Status;

use crate::scanners::{
    connection_value, default_profile_label, humanize_backend, status_summary, status_value,
};
use crate::store::ScannerEntry;

/// The status word's colour, and the only place a `Status` becomes one.
///
/// `Offline` is deliberately uncoloured: the design gives green to *Online* and leaves
/// the absence of a scanner as plain text, so the pane reads as one accent rather than a
/// row of traffic lights.
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

fn value_label() -> gtk::Label {
    let label = gtk::Label::new(None);
    label.set_xalign(1.0);
    label.set_halign(gtk::Align::End);
    label.set_wrap(true);
    label
}

/// One middle-section row: icon, label, and the value pushed to the right margin.
fn fact_row(icon_name: &str, title: &str, value: &gtk::Label) -> gtk::Box {
    let icon = gtk::Image::from_icon_name(icon_name);
    icon.add_css_class("dim-label");

    let title_label = gtk::Label::new(Some(title));
    title_label.set_xalign(0.0);

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.append(&icon);
    row.append(&title_label);
    row.append(&gtk::Box::builder().hexpand(true).build());
    row.append(value);
    row
}

pub struct DetailsPane {
    root: gtk::Box,
    icon: gtk::Image,
    name: gtk::Label,
    /// The status word of the summary line — the half that carries the colour.
    summary_status: gtk::Label,
    /// The rest of the summary line, which never does.
    summary_rest: gtk::Label,
    status: gtk::Label,
    connection: gtk::Label,
    address: gtk::Label,
    backend: gtk::Label,
    default_profile: gtk::Label,
    configure_list: gtk::ListBox,
    unpair: gtk::Button,
    /// The bottom section, hidden whole for a scanner that is not paired yet: both
    /// actions in it are pairing-only, and a lone rule over empty space is not a section.
    actions: gtk::Box,
    actions_separator: gtk::Separator,
}

impl Default for DetailsPane {
    fn default() -> Self {
        Self::new()
    }
}

impl DetailsPane {
    pub fn new() -> Self {
        let icon = gtk::Image::from_icon_name(identity_icon_name(""));
        icon.set_pixel_size(80);
        icon.set_halign(gtk::Align::Center);

        let name = gtk::Label::new(None);
        name.add_css_class("title-2");
        name.set_wrap(true);
        name.set_justify(gtk::Justification::Center);

        let summary_status = gtk::Label::new(None);
        let summary_rest = gtk::Label::new(None);
        let summary = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        summary.set_halign(gtk::Align::Center);
        summary.append(&summary_status);
        summary.append(&summary_rest);

        // The box fills the pane so a long name has the full width to wrap into; each
        // child centres itself.
        let identity = gtk::Box::new(gtk::Orientation::Vertical, 12);
        identity.append(&icon);
        identity.append(&name);
        identity.append(&summary);

        let status = value_label();
        let connection = value_label();
        let address = value_label();
        address.set_selectable(true);
        let backend = value_label();
        let default_profile = value_label();

        let facts = gtk::Box::new(gtk::Orientation::Vertical, 18);
        let rows = [
            (
                "network-cellular-signal-excellent-symbolic",
                "Status",
                &status,
            ),
            (
                "network-wireless-hotspot-symbolic",
                "Connection",
                &connection,
            ),
            ("network-wired-symbolic", "Address", &address),
            ("application-x-sharedlib-symbolic", "Backend", &backend),
            (
                "text-x-generic-symbolic",
                "Default profile",
                &default_profile,
            ),
        ];
        for (icon_name, title, value) in rows {
            facts.append(&fact_row(icon_name, title, value));
        }

        // `boxed-list` is what gives the row the lighter-than-the-pane background the
        // paired scanner list already has; a plain button would sit on the pane colour.
        let configure = adw::ActionRow::builder()
            .title("Configure buttons")
            .activatable(true)
            .build();
        configure.add_prefix(&gtk::Image::from_icon_name("preferences-other-symbolic"));
        configure.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));

        let configure_list = gtk::ListBox::new();
        configure_list.add_css_class("boxed-list");
        configure_list.set_selection_mode(gtk::SelectionMode::None);
        configure_list.append(&configure);

        // Not in the mockup, and kept anyway: unpairing is reachable from nowhere else in
        // the window, and §7 of the GUI design owes it a confirmation dialog. It sits
        // below the row rather than in the same list so a mis-aimed click cannot land on
        // the irreversible half of the section.
        let unpair = gtk::Button::with_label("Unpair");
        unpair.add_css_class("destructive-action");
        unpair.set_halign(gtk::Align::Fill);

        let actions = gtk::Box::new(gtk::Orientation::Vertical, 12);
        actions.append(&configure_list);
        actions.append(&unpair);
        actions.set_visible(false);

        let actions_separator = gtk::Separator::new(gtk::Orientation::Horizontal);
        actions_separator.set_visible(false);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 24);
        root.set_margin_top(24);
        root.set_margin_bottom(24);
        root.set_margin_start(24);
        root.set_margin_end(24);
        root.append(&identity);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        root.append(&facts);
        root.append(&actions_separator);
        root.append(&actions);

        Self {
            root,
            icon,
            name,
            summary_status,
            summary_rest,
            status,
            connection,
            address,
            backend,
            default_profile,
            configure_list,
            unpair,
            actions,
            actions_separator,
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn connect_configure<F>(&self, callback: F)
    where
        F: Fn() + 'static,
    {
        // Wired on the list rather than on the row: `row-activated` is what a plain
        // `GtkListBox` emits for a click, with no `AdwPreferencesGroup` in between to
        // forward it to the row's own activation.
        self.configure_list
            .connect_row_activated(move |_, _| callback());
    }

    pub fn connect_unpair<F>(&self, callback: F)
    where
        F: Fn() + 'static,
    {
        self.unpair.connect_clicked(move |_| callback());
    }

    pub fn render(&self, scanner: &ScannerEntry) {
        let state = &scanner.state;

        self.icon
            .set_icon_name(Some(identity_icon_name(&state.backend)));
        self.name.set_label(&state.name);

        let (lead, rest) = status_summary(state.status, state.connected, state.paired);
        self.summary_status.set_label(lead);
        self.summary_rest.set_label(rest);
        // An unpaired scanner leads with "Discovered", which is not a status and takes no
        // colour even when the device itself is online.
        set_status_class(&self.summary_status, state.paired.then_some(state.status));

        self.status.set_label(status_value(state.status));
        set_status_class(&self.status, Some(state.status));
        self.connection.set_label(connection_value(state.connected));
        self.address.set_label(&state.address);
        self.backend.set_label(&humanize_backend(&state.backend));
        self.default_profile
            .set_label(&default_profile_label(state.default_profile));

        self.actions.set_visible(state.paired);
        self.actions_separator.set_visible(state.paired);
    }

    pub fn clear(&self) {
        self.icon.set_icon_name(Some(identity_icon_name("")));
        self.name.set_label("");
        self.summary_status.set_label("");
        self.summary_rest.set_label("");
        set_status_class(&self.summary_status, None);
        self.status.set_label("");
        set_status_class(&self.status, None);
        self.connection.set_label("");
        self.address.set_label("");
        self.backend.set_label("");
        self.default_profile.set_label("");
        self.actions.set_visible(false);
        self.actions_separator.set_visible(false);
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
    use std::collections::BTreeMap;

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

        pane.render(&scanner("proprietary:brother", Status::Online, true, true));

        assert_eq!(pane.icon.icon_name().as_deref(), Some("printer-symbolic"));
        assert_eq!(pane.name.label(), "Brother MFC-L2710DW");
        assert_eq!(pane.summary_status.label(), "Online");
        assert_eq!(pane.summary_rest.label(), " • Connected");
        assert!(
            pane.summary_status.has_css_class("success"),
            "the design gives the online summary its one accent"
        );
        assert_eq!(pane.status.label(), "Online");
        assert_eq!(pane.connection.label(), "Connected");
        assert_eq!(pane.address.label(), "192.168.1.23");
        assert_eq!(pane.backend.label(), "Brother");
        assert_eq!(pane.default_profile.label(), "Document");

        // Paired is what the bottom section is for; nothing to configure otherwise.
        assert!(pane.configure_list.is_visible());
        assert!(pane.unpair.is_visible());

        // Offline drops the accent rather than turning it another colour.
        pane.render(&scanner("hplip", Status::Offline, false, true));
        assert_eq!(pane.summary_status.label(), "Offline");
        assert_eq!(pane.summary_rest.label(), "");
        assert!(!pane.summary_status.has_css_class("success"));
        assert!(!pane.status.has_css_class("success"));
        assert_eq!(pane.connection.label(), "Disconnected");
        assert_eq!(pane.backend.label(), "HPLIP");

        // A discovered scanner leads with a word that is not a status, so it takes no
        // colour even though the device is online, and offers neither action.
        pane.render(&scanner("mobile", Status::Online, false, false));
        assert_eq!(pane.icon.icon_name().as_deref(), Some("phone-symbolic"));
        assert_eq!(pane.summary_status.label(), "Discovered");
        assert_eq!(pane.summary_rest.label(), " • Not paired");
        assert!(!pane.summary_status.has_css_class("success"));
        assert!(pane.status.has_css_class("success"), "the device is online");
        assert_eq!(pane.backend.label(), "Mobile");
        assert!(!pane.configure_list.is_visible());
        assert!(!pane.unpair.is_visible());

        // Deselection empties the pane instead of leaving the last scanner on screen.
        pane.clear();
        assert_eq!(pane.name.label(), "");
        assert_eq!(pane.summary_status.label(), "");
        assert_eq!(pane.status.label(), "");
        assert_eq!(pane.address.label(), "");
        assert!(!pane.configure_list.is_visible());
        assert!(!pane.unpair.is_visible());
    }
}
