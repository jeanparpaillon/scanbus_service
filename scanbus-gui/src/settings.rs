//! The Settings page — the composite-template subclass `settings-page.blp` declares.
//!
//! Split out of `window.rs` rather than declared as ids inside `window.blp`: the rows
//! below have nothing to do with the window's own chrome, and keeping them here is what
//! stopped `window.rs` regrowing the 200-line `settings_page` function this replaced.

use std::cell::{Cell, OnceCell};
use std::rc::Rc;

use async_channel::Sender;
use gtk::{CompositeTemplate, TemplateChild, glib};
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use libadwaita::subclass::prelude::*;

use crate::autostart;
use crate::bus::BusCommand;
use crate::scanners::ScannerListModel;
use crate::store::ServiceState;
use crate::window::ScanbusWindow;

mod imp {
    use super::*;

    // No `Debug`, for the reason `window.rs` gives: `Rc<ScannerListModel>` does not
    // derive it.
    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/org/scanbus/Gui/ui/settings-page.ui")]
    pub struct SettingsPage {
        #[template_child]
        pub daemon_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub daemon_start_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub daemon_version_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub daemon_backends_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub daemon_profile_types_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub output_image_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub output_document_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub autostart_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub autostart_toggle: TemplateChild<gtk::Switch>,

        /// Two of the three handles `ScanbusWindow` holds. The lifecycle is not among
        /// them: nothing on this page starts, holds or releases the background hold, so
        /// it would be a field no callback reads.
        pub model: OnceCell<Rc<ScannerListModel>>,
        pub commands: OnceCell<Sender<BusCommand>>,

        /// Set across the `set_active` that puts the switch back after a failed write,
        /// because that assignment raises `notify::active` again and the handler would
        /// otherwise retry the write it has just been told failed.
        pub syncing: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SettingsPage {
        /// Must match `template $SettingsPage` in `settings-page.blp`.
        const NAME: &'static str = "SettingsPage";
        type Type = super::SettingsPage;
        // `Adw.Bin`, not `Gtk.ScrolledWindow`: gtk4-rs 0.10 has no `ScrolledWindowImpl`
        // and no `IsSubclassable` impl for it, so the scroller cannot be a parent type.
        // It is the bin's child in the template instead; see the note in the `.blp`.
        type ParentType = adw::Bin;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            // Instance callbacks: the `#[gtk::template_callbacks]` block is on the
            // wrapper type, as it is in `window.rs`.
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for SettingsPage {}
    impl WidgetImpl for SettingsPage {}
    impl BinImpl for SettingsPage {}
}

glib::wrapper! {
    /// The Settings page, built from `settings-page.blp` and written from the store.
    pub struct SettingsPage(ObjectSubclass<imp::SettingsPage>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl SettingsPage {
    /// Builds the page and hands it the two handles its callbacks need.
    ///
    /// The initial autostart read happens here rather than in a `render`: it is the one
    /// piece of this page's content that does not come from the store, so no
    /// `connect_changed` would ever run it.
    pub fn new(model: Rc<ScannerListModel>, commands: Sender<BusCommand>) -> Self {
        let page: Self = glib::Object::new();

        let imp = page.imp();
        assert!(imp.model.set(model).is_ok(), "model set twice");
        assert!(imp.commands.set(commands).is_ok(), "commands set twice");

        // A failure here is not fatal: the switch reports what stopped it being read and
        // desensitises itself, which is what the function this replaced did too.
        match autostart::is_enabled() {
            Ok(enabled) => {
                imp.autostart_toggle.set_active(enabled);
                imp.autostart_row.set_subtitle(&autostart_subtitle(enabled));
            }
            Err(error) => {
                imp.autostart_toggle.set_sensitive(false);
                imp.autostart_row
                    .set_subtitle(&format!("Could not read the autostart override: {error}"));
            }
        }

        // Registered once, here, rather than from a `#[template_callback]`: `render` is
        // driven by the store rather than by a signal on any widget of this page. The
        // closure holds a weak reference because `ScannerListModel` keeps every callback
        // it is given for its own lifetime, and the model is held in `imp` — a strong
        // `self` would be a cycle through the store, and the page would outlive the
        // window that showed it.
        let weak = page.downgrade();
        imp.model
            .get()
            .expect("model set just above")
            .connect_changed(move || {
                if let Some(page) = weak.upgrade() {
                    page.render();
                }
            });

        // The store has usually answered `GetAll` long before Settings is first shown, so
        // without this first call the rows would sit empty until something else changed.
        page.render();

        page
    }

    /// Writes every row on this page from the store.
    ///
    /// The autostart row is the one row not written here: it comes from the XDG override,
    /// which no store change can move, so `new` reads it once instead.
    fn render(&self) {
        let imp = self.imp();
        let model = imp.model.get().expect("model set in `new`");
        let details = model.service_details();

        match model.service_state() {
            ServiceState::Running => {
                imp.daemon_row.set_subtitle("Running");
                imp.daemon_start_button.set_visible(false);
            }
            ServiceState::Activatable => {
                imp.daemon_row.set_subtitle("Installed, but not running");
                imp.daemon_start_button.set_visible(true);
                imp.daemon_start_button.set_sensitive(true);
            }
            ServiceState::Absent => {
                imp.daemon_row.set_subtitle("Absent from this session bus");
                imp.daemon_start_button.set_visible(false);
            }
            ServiceState::Unknown => {
                imp.daemon_row.set_subtitle("Checking the bus…");
                imp.daemon_start_button.set_visible(false);
            }
        }

        imp.daemon_version_row
            .set_subtitle(details.version.as_deref().unwrap_or("Unknown"));
        imp.daemon_backends_row
            .set_subtitle(&joined(&details.backends));
        imp.daemon_profile_types_row
            .set_subtitle(&joined(&details.profile_types));
        set_folder_row(
            &imp.output_image_row,
            details
                .output_folders
                .get(&scanbus_core::ProfileKind::Image),
        );
        set_folder_row(
            &imp.output_document_row,
            details
                .output_folders
                .get(&scanbus_core::ProfileKind::Document),
        );
    }

    /// The window this page is shown in, or `None` before it has been put in one.
    ///
    /// Looked up rather than held: the window owns the stack that owns this page, so a
    /// stored handle would be a strong reference back up its own parent chain. The
    /// `None` arm is what a bare instantiation — the `gtk-tests` one — gets.
    // Called only from the template callbacks below.
    fn window(&self) -> Option<ScanbusWindow> {
        self.root()?.downcast::<ScanbusWindow>().ok()
    }
}

/// The five handlers `settings-page.blp` names, in the order the template mentions them.
///
/// Each is declared `swapped` there, which is what puts the page in `&self`; the emitting
/// widget is the argument that gets dropped.
#[gtk::template_callbacks]
impl SettingsPage {
    /// Asks the bus thread to start the daemon. The row's own text follows the store, so
    /// nothing here writes it.
    #[template_callback]
    fn on_start_service(&self) {
        let commands = self.imp().commands.get().expect("commands set in `new`");
        let _ = commands.try_send(BusCommand::StartService);
    }

    /// Both output rows: jump to the Profiles page, where the folder is editable.
    #[template_callback]
    fn on_show_profiles(&self) {
        if let Some(window) = self.window() {
            window.show_page("profiles");
        }
    }

    /// Writes the XDG autostart override, and puts the switch back when the write fails.
    ///
    /// The revert raises `notify::active` a second time, which is what `syncing` guards:
    /// without it the handler would see the reverted value as a fresh request and try to
    /// write the opposite of what just failed.
    #[template_callback]
    fn on_autostart_toggled(&self) {
        let imp = self.imp();
        if imp.syncing.get() {
            return;
        }

        let enabled = imp.autostart_toggle.is_active();
        if let Err(error) = autostart::set_enabled(enabled) {
            imp.syncing.set(true);
            imp.autostart_toggle.set_active(!enabled);
            imp.syncing.set(false);
            if let Some(window) = self.window() {
                window.add_toast(adw::Toast::new(&format!(
                    "Could not update the autostart override: {error}"
                )));
            }
            return;
        }

        imp.autostart_row.set_subtitle(&autostart_subtitle(enabled));
    }

    /// Builds the About dialog at runtime: its GUI version and repository come from the
    /// crate's own `env!()` values and the daemon version from the store, so none of it
    /// can be declared in the `.blp`.
    #[template_callback]
    fn on_about(&self) {
        let Some(window) = self.window() else {
            return;
        };
        let model = self.imp().model.get().expect("model set in `new`");

        let details = model.service_details();
        let daemon_version = details.version.unwrap_or_else(|| "unknown".to_owned());
        let about = gtk::AboutDialog::builder()
            .program_name("Scanbus")
            .logo_icon_name("org.scanbus.Gui")
            .version(env!("CARGO_PKG_VERSION"))
            .website(env!("CARGO_PKG_REPOSITORY"))
            .website_label("Repository")
            .license_type(gtk::License::Unknown)
            .comments(format!(
                "GUI version: {}.\nDaemon version: {}.\nLicence: Unknown.",
                env!("CARGO_PKG_VERSION"),
                daemon_version
            ))
            .transient_for(&window)
            .modal(true)
            .build();
        about.present();
    }
}

/// The three content-derivation helpers this page writes its subtitles with.
///
/// Free functions rather than methods, and kept apart from `render`: each is a pure
/// mapping from store data to the string a row shows, which is what makes them the part
/// of this page that can be tested without a display. The tests are at the foot of this
/// file.
pub(crate) fn autostart_subtitle(enabled: bool) -> String {
    if enabled {
        format!(
            "Enabled. Turn this off by writing {} with Hidden=true.",
            autostart::DESKTOP_FILE_NAME
        )
    } else {
        format!(
            "Disabled through ~/.config/autostart/{}. Turn it back on to use the packaged GNOME autostart entry again.",
            autostart::DESKTOP_FILE_NAME
        )
    }
}

pub(crate) fn joined(values: &[String]) -> String {
    if values.is_empty() {
        "-".to_owned()
    } else {
        values.join(", ")
    }
}

pub(crate) fn set_folder_row(row: &adw::ActionRow, folder: Option<&String>) {
    row.set_subtitle(folder.map(String::as_str).unwrap_or("Unknown"));
}

/// The two helpers above that derive a string and touch no widget, so they need no
/// display and stay outside the `gtk-tests` suite.
///
/// `set_folder_row` is the third, and it writes to an `adw::ActionRow`; its check is in
/// [`widget_checks`] below, where GTK has been initialised.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_subtitle_names_the_override_file_either_way() {
        // Enabled reads as "the packaged entry is in force", and the way out of it is
        // the file the disabled subtitle names — so the file name has to appear in
        // both, or the row tells the user how to change nothing.
        let enabled = autostart_subtitle(true);
        assert!(
            enabled.starts_with("Enabled."),
            "enabled subtitle should say so first: {enabled}"
        );
        assert!(
            enabled.contains(autostart::DESKTOP_FILE_NAME),
            "enabled subtitle should name the override file: {enabled}"
        );

        let disabled = autostart_subtitle(false);
        assert!(
            disabled.starts_with("Disabled"),
            "disabled subtitle should say so first: {disabled}"
        );
        assert!(
            disabled.contains(autostart::DESKTOP_FILE_NAME),
            "disabled subtitle should name the override file: {disabled}"
        );
        assert_ne!(enabled, disabled);
    }

    #[test]
    fn joined_writes_a_dash_for_nothing_rather_than_an_empty_row() {
        // An empty subtitle would read as "not answered yet", which is a different
        // thing from a daemon that answered with an empty list.
        assert_eq!(joined(&[]), "-");
        assert_eq!(joined(&["sane".to_owned()]), "sane");
        assert_eq!(
            joined(&["sane".to_owned(), "brother".to_owned()]),
            "sane, brother"
        );
    }
}

/// The Settings page's half of the one GTK test — see [`crate::gtk_tests`].
///
/// Two things, in the order they appear: `set_folder_row`, the one content helper
/// that takes a widget and so cannot live in the display-free tests above, and then
/// the page itself, instantiated once as §10 of the design doc asks — which is what
/// makes a `settings-page.blp` id renamed without its `#[template_child]` a test
/// failure rather than a widget that silently never appears.
#[cfg(all(test, feature = "gtk-tests"))]
pub(crate) mod widget_checks {
    use super::*;
    // Only the instantiation below moves the store, so these two stay out of the
    // module-level imports rather than being `cfg`-gated there.
    use crate::store::{ServiceDetails, StoreEvent};

    pub(crate) fn run() {
        // The third content-derivation helper. It needs a real row, because the whole of
        // what it decides is what a row shows when the daemon has not named a folder:
        // "Unknown" rather than the empty subtitle an `Option` written straight through
        // would leave.
        let row = adw::ActionRow::new();

        set_folder_row(&row, None);
        assert_eq!(row.subtitle().unwrap_or_default().as_str(), "Unknown");

        let folder = "/home/someone/Pictures".to_owned();
        set_folder_row(&row, Some(&folder));
        assert_eq!(row.subtitle().unwrap_or_default().as_str(), folder.as_str());

        // A folder that stops being advertised goes back to "Unknown" rather than
        // leaving the last one on screen as though it were still in force.
        set_folder_row(&row, None);
        assert_eq!(row.subtitle().unwrap_or_default().as_str(), "Unknown");

        // The instantiation §10 of the design doc asks for, and the same drift check the
        // window's own is: `init_template` fills the nine `#[template_child]`s above from
        // the ids in `settings-page.blp`, so one renamed or dropped there fails here
        // rather than on a page a user opens.
        let model = Rc::new(ScannerListModel::new());
        // The receiver is kept: a dropped one closes the channel, and `on_start_service`
        // ignores a failed `try_send` by design.
        let (commands, sent) = async_channel::unbounded();
        let page = SettingsPage::new(Rc::clone(&model), commands);
        let imp = page.imp();

        // A store that has not answered the bus yet. Every row says which of the four
        // states that is rather than sitting empty, and there is nothing to start.
        assert_eq!(
            imp.daemon_row.subtitle().unwrap_or_default().as_str(),
            "Checking the bus…"
        );
        assert!(!imp.daemon_start_button.get_visible());
        assert_eq!(
            imp.daemon_version_row
                .subtitle()
                .unwrap_or_default()
                .as_str(),
            "Unknown"
        );
        assert_eq!(
            imp.daemon_backends_row
                .subtitle()
                .unwrap_or_default()
                .as_str(),
            "-"
        );

        // The store moving is the only thing that writes these rows: `new` registers a
        // `connect_changed` and this is what says the registration survived.
        model
            .apply_event(StoreEvent::ServiceState(ServiceState::Activatable))
            .expect("a service state carries nothing to decode");
        assert_eq!(
            imp.daemon_row.subtitle().unwrap_or_default().as_str(),
            "Installed, but not running"
        );
        assert!(imp.daemon_start_button.get_visible());
        assert!(imp.daemon_start_button.get_sensitive());

        // Clicked through the template, which is also the check that `swapped` is still
        // on the handler: without it `self` would be the button and the bus channel
        // would be out of reach.
        imp.daemon_start_button.emit_clicked();
        assert!(
            matches!(sent.try_recv(), Ok(BusCommand::StartService)),
            "the Start button should have asked the bus thread to start the daemon"
        );

        model
            .apply_event(StoreEvent::ServiceDetails(ServiceDetails {
                version: Some("0.1.0".to_owned()),
                backends: vec!["sane".to_owned(), "mobile".to_owned()],
                profile_types: vec!["image".to_owned()],
                output_folders: std::collections::BTreeMap::from([(
                    scanbus_core::ProfileKind::Image,
                    "/home/someone/Pictures".to_owned(),
                )]),
            }))
            .expect("service details carry nothing to decode");
        assert_eq!(
            imp.daemon_version_row
                .subtitle()
                .unwrap_or_default()
                .as_str(),
            "0.1.0"
        );
        assert_eq!(
            imp.daemon_backends_row
                .subtitle()
                .unwrap_or_default()
                .as_str(),
            "sane, mobile"
        );
        assert_eq!(
            imp.daemon_profile_types_row
                .subtitle()
                .unwrap_or_default()
                .as_str(),
            "image"
        );
        assert_eq!(
            imp.output_image_row.subtitle().unwrap_or_default().as_str(),
            "/home/someone/Pictures"
        );
        // The daemon named no document folder, so that row says so rather than keeping
        // the image one's.
        assert_eq!(
            imp.output_document_row
                .subtitle()
                .unwrap_or_default()
                .as_str(),
            "Unknown"
        );

        // Not in a window, which is the arm `SettingsPage::window` documents: the two
        // output rows' jump to Profiles has to be a no-op rather than a panic here.
        assert!(page.window().is_none());
        page.on_show_profiles();
    }
}
