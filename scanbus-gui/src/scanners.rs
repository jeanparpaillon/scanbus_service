use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use async_channel::Sender;
use gio::ListStore;
use gtk::gio;
use gtk::glib;
use gtk::{CompositeTemplate, TemplateChild};
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
// Brings in gtk4's subclass prelude, and glib's through it, so `BoxImpl` arrives
// alongside the `ObjectSubclass`/`ObjectImpl` the two subclasses in this file need.
use libadwaita::subclass::prelude::*;
use scanbus_core::{PairingState, ProfileKind, Status};

use crate::bus::BusCommand;
use crate::buttons::ButtonsPage;
use crate::details::DetailsPane;
use crate::scanner_row::ScannerRow;
use crate::store::{
    DiscoveryState, ProfileEntry, ScannerEntry, ServiceDetails, ServiceState, Store, StoreEvent,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToastAction {
    RetryPair { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastSpec {
    pub message: String,
    pub button_label: Option<String>,
    pub action: Option<ToastAction>,
}

impl ToastSpec {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            button_label: None,
            action: None,
        }
    }

    pub fn retry_pair(message: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            button_label: Some("Retry".to_owned()),
            action: Some(ToastAction::RetryPair { path: path.into() }),
        }
    }
}

mod scanner_object {
    use super::*;

    #[derive(Default, glib::Properties)]
    #[properties(wrapper_type = super::ScannerObject)]
    pub struct ScannerObject {
        #[property(get, set, type = String)]
        pub path: RefCell<String>,
        #[property(get, set, type = String)]
        pub name: RefCell<String>,
        #[property(get, set, type = String)]
        pub address: RefCell<String>,
        #[property(get, set, type = String)]
        pub backend: RefCell<String>,
        #[property(get, set, type = String)]
        pub default_profile: RefCell<String>,
        #[property(get, set, type = bool)]
        pub paired: Cell<bool>,
        #[property(get, set, type = bool)]
        pub connected: Cell<bool>,
        #[property(get, set, type = String)]
        pub status: RefCell<String>,
        #[property(get, set, type = String)]
        pub status_line: RefCell<String>,
        #[property(get, set, type = String)]
        pub pairing_state: RefCell<String>,
        #[property(get, set, type = String)]
        pub pairing_error: RefCell<String>,
        #[property(get, set, type = String)]
        pub pairing_code: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ScannerObject {
        const NAME: &'static str = "ScanbusGuiScannerObject";
        type Type = super::ScannerObject;
    }

    #[glib::derived_properties]
    impl ObjectImpl for ScannerObject {}
}

glib::wrapper! {
    pub struct ScannerObject(ObjectSubclass<scanner_object::ScannerObject>);
}

impl ScannerObject {
    fn new(entry: &ScannerEntry) -> Self {
        glib::Object::builder::<Self>()
            .property("path", &entry.path)
            .property("name", &entry.state.name)
            .property("address", &entry.state.address)
            .property("backend", &entry.state.backend)
            .property(
                "default-profile",
                default_profile_label(entry.state.default_profile),
            )
            .property("paired", entry.state.paired)
            .property("connected", entry.state.connected)
            .property("status", entry.state.status.as_str())
            .property(
                "status-line",
                status_line(
                    entry.state.status,
                    entry.state.connected,
                    entry.state.paired,
                ),
            )
            .property("pairing-state", pairing_state_name(&entry.state.pairing))
            .property("pairing-error", pairing_error(&entry.state.pairing))
            .property("pairing-code", pairing_code(&entry.state.pairing))
            .build()
    }

    fn update_from_entry(&self, entry: &ScannerEntry) {
        self.set_property("path", &entry.path);
        self.set_property("name", &entry.state.name);
        self.set_property("address", &entry.state.address);
        self.set_property("backend", &entry.state.backend);
        self.set_property(
            "default-profile",
            default_profile_label(entry.state.default_profile),
        );
        self.set_property("paired", entry.state.paired);
        self.set_property("connected", entry.state.connected);
        self.set_property("status", entry.state.status.as_str());
        self.set_property(
            "status-line",
            status_line(
                entry.state.status,
                entry.state.connected,
                entry.state.paired,
            ),
        );
        self.set_property("pairing-state", pairing_state_name(&entry.state.pairing));
        self.set_property("pairing-error", pairing_error(&entry.state.pairing));
        self.set_property("pairing-code", pairing_code(&entry.state.pairing));
    }
}

/// The two halves of a scanner's one-line summary: the status word, which the detail
/// pane colours, and the rest of the line, which it never does.
pub(crate) fn status_summary(
    status: Status,
    connected: bool,
    paired: bool,
) -> (&'static str, &'static str) {
    if !paired {
        return ("Discovered", " • Not paired");
    }

    (
        status_value(status),
        if connected { " • Connected" } else { "" },
    )
}

pub fn status_line(status: Status, connected: bool, paired: bool) -> String {
    let (lead, rest) = status_summary(status, connected, paired);
    format!("{lead}{rest}")
}

pub(crate) fn default_profile_label(profile: Option<ProfileKind>) -> String {
    profile.map_or_else(
        || "None".to_owned(),
        |profile| humanize_profile(profile.as_str()),
    )
}

pub fn humanize_profile(profile: &str) -> String {
    // Two of the four kinds are not simply capitalised words.
    match profile {
        "ocr" => return "OCR".to_owned(),
        "email" => return "E-mail".to_owned(),
        _ => {}
    }

    let mut chars = profile.chars();
    if let Some(first) = chars.next() {
        let mut label = first.to_uppercase().collect::<String>();
        label.push_str(chars.as_str());
        label
    } else {
        "None".to_owned()
    }
}

pub(crate) fn status_value(status: Status) -> &'static str {
    match status {
        Status::Offline => "Offline",
        Status::Online => "Online",
        Status::Busy => "Busy",
        Status::Error => "Error",
    }
}

/// What the detail pane's *Connection* row says.
///
/// The verb is the user's, not the daemon's: the switch that turns the host listener on
/// lives on the Configure buttons page and still says so ([`connection_subtitle`]), but
/// the fact this row states is whether scanner and host are talking.
pub(crate) fn connection_value(connected: bool) -> &'static str {
    if connected {
        "Connected"
    } else {
        "Disconnected"
    }
}

pub fn connection_subtitle(status: Status, connected: bool) -> &'static str {
    if status == Status::Offline {
        "Unavailable while the scanner is offline"
    } else if connected {
        "This host is ready to receive from this scanner"
    } else {
        "Turn on to let this host receive from this scanner"
    }
}

pub fn connection_banner(status: Status, connected: bool, paired: bool) -> String {
    let paired = if paired { "Paired" } else { "Not paired" };
    let online = status_value(status);
    let listening = if connected {
        "Host listening"
    } else {
        "Host not listening"
    };
    format!("{paired} • {online} • {listening}")
}

type Callback = Box<dyn Fn() + 'static>;
type ToastCallback = Box<dyn Fn(ToastSpec) + 'static>;

pub struct ScannerListModel {
    store: RefCell<Store>,
    list: ListStore,
    by_path: RefCell<HashMap<String, ScannerObject>>,
    selected_path: RefCell<Option<String>>,
    discovery: Cell<DiscoveryState>,
    callbacks: RefCell<Vec<Callback>>,
    toast_callbacks: RefCell<Vec<ToastCallback>>,
}

impl ScannerListModel {
    pub fn new() -> Self {
        Self {
            store: RefCell::new(Store::default()),
            list: ListStore::new::<ScannerObject>(),
            by_path: RefCell::new(HashMap::new()),
            selected_path: RefCell::new(None),
            discovery: Cell::new(DiscoveryState::Idle),
            callbacks: RefCell::new(Vec::new()),
            toast_callbacks: RefCell::new(Vec::new()),
        }
    }

    pub fn list(&self) -> ListStore {
        self.list.clone()
    }

    pub fn service_state(&self) -> ServiceState {
        self.store.borrow().service
    }

    pub fn service_details(&self) -> ServiceDetails {
        self.store.borrow().details.clone()
    }

    pub fn selected_path(&self) -> Option<String> {
        self.selected_path.borrow().clone()
    }

    pub fn discovery_state(&self) -> DiscoveryState {
        self.discovery.get()
    }

    pub fn discovered_count(&self) -> usize {
        self.store
            .borrow()
            .scanners
            .values()
            .filter(|entry| !entry.state.paired)
            .count()
    }

    pub fn has_scanners(&self) -> bool {
        !self.store.borrow().scanners.is_empty()
    }

    pub fn connect_changed<F>(&self, callback: F)
    where
        F: Fn() + 'static,
    {
        self.callbacks.borrow_mut().push(Box::new(callback));
    }

    pub fn connect_toast<F>(&self, callback: F)
    where
        F: Fn(ToastSpec) + 'static,
    {
        self.toast_callbacks.borrow_mut().push(Box::new(callback));
    }

    pub fn set_selected_path(&self, selected_path: Option<String>) {
        if *self.selected_path.borrow() == selected_path {
            return;
        }

        *self.selected_path.borrow_mut() = selected_path;
        self.notify_changed();
    }

    pub fn begin_discovery_start(&self) -> bool {
        if self.discovery.get() != DiscoveryState::Idle {
            return false;
        }

        self.discovery.set(DiscoveryState::Starting);
        self.notify_changed();
        true
    }

    pub fn mark_discovery_active(&self) {
        self.set_discovery_state(DiscoveryState::Active);
    }

    pub fn begin_discovery_stop(&self) -> bool {
        match self.discovery.get() {
            DiscoveryState::Starting | DiscoveryState::Active => {
                self.discovery.set(DiscoveryState::Stopping);
                self.notify_changed();
                true
            }
            DiscoveryState::Idle | DiscoveryState::Stopping => false,
        }
    }

    pub fn mark_discovery_idle(&self) {
        self.set_discovery_state(DiscoveryState::Idle);
    }

    pub fn emit_toast_spec(&self, toast: ToastSpec) {
        for callback in self.toast_callbacks.borrow().iter() {
            callback(toast.clone());
        }
    }

    pub fn scanner(&self, path: &str) -> Option<ScannerEntry> {
        self.store
            .borrow()
            .scanners
            .values()
            .find(|entry| entry.path == path)
            .cloned()
    }

    /// The `Profile1` objects, which the buttons page needs to tell a per-key override
    /// from a value the profile already had.
    pub fn profiles(&self) -> BTreeMap<ProfileKind, ProfileEntry> {
        self.store.borrow().profiles.clone()
    }

    /// Re-renders every view from the store without changing it.
    ///
    /// This is the revert half of a refused write: the widget the user touched showed
    /// what they picked, the store never moved, and rendering from it again puts the
    /// widget back. `main.rs` calls it whenever a toast arrives.
    pub fn refresh(&self) {
        self.notify_changed();
    }

    pub fn apply_event(&self, event: StoreEvent) -> Result<(), scanbus_client::DecodeError> {
        self.store.borrow_mut().apply(event)?;
        if self.store.borrow().service == ServiceState::Absent {
            self.discovery.set(DiscoveryState::Idle);
        }
        self.sync_objects();
        self.notify_changed();
        Ok(())
    }

    fn sync_objects(&self) {
        let store = self.store.borrow();
        let desired: Vec<&ScannerEntry> = store.scanners.values().collect();
        let desired_paths: HashSet<&str> =
            desired.iter().map(|entry| entry.path.as_str()).collect();

        let mut removed = Vec::new();
        {
            let by_path = self.by_path.borrow();
            for path in by_path.keys() {
                if !desired_paths.contains(path.as_str()) {
                    removed.push(path.clone());
                }
            }
        }

        for path in removed {
            if let Some(position) = self.find_position(&path) {
                self.list.remove(position);
            }
            self.by_path.borrow_mut().remove(&path);
            if self.selected_path.borrow().as_deref() == Some(path.as_str()) {
                *self.selected_path.borrow_mut() = None;
            }
        }

        for (index, entry) in desired.into_iter().enumerate() {
            let index = index as u32;
            let path = entry.path.clone();

            if let Some(existing) = self.by_path.borrow().get(&path).cloned() {
                existing.update_from_entry(entry);
                if self.list.item(index).as_ref() != Some(existing.upcast_ref()) {
                    if let Some(current) = self.find_position(&path) {
                        self.list.remove(current);
                    }
                    self.list.insert(index, &existing);
                }
                continue;
            }

            let scanner = ScannerObject::new(entry);
            self.list.insert(index, &scanner);
            self.by_path.borrow_mut().insert(path, scanner);
        }
    }

    fn find_position(&self, path: &str) -> Option<u32> {
        (0..self.list.n_items()).find(|index| {
            self.list
                .item(*index)
                .and_then(|item| item.downcast::<ScannerObject>().ok())
                .is_some_and(|scanner| scanner.path() == path)
        })
    }

    fn notify_changed(&self) {
        for callback in self.callbacks.borrow().iter() {
            callback();
        }
    }

    fn set_discovery_state(&self, discovery: DiscoveryState) {
        if self.discovery.replace(discovery) != discovery {
            self.notify_changed();
        }
    }
}

/// The private instance struct GObject owns for the pane; [`ScannersPane`] is the handle
/// `ScanbusWindow` holds.
mod scanners_pane {
    use super::*;

    // No `Debug`, for the reason `window.rs` gives: half the handles below are plain Rust
    // types that do not derive it.
    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/org/scanbus/Gui/ui/scanners-pane.ui")]
    pub struct ScannersPane {
        #[template_child]
        pub service_banner: TemplateChild<gtk::Revealer>,
        #[template_child]
        pub scanners_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub paired_group: TemplateChild<adw::PreferencesGroup>,
        #[template_child]
        pub paired_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub discovered_group: TemplateChild<adw::PreferencesGroup>,
        #[template_child]
        pub discovered_list: TemplateChild<gtk::ListBox>,
        /// Declared with its placeholder page only; `new` adds the other two.
        #[template_child]
        pub detail_stack: TemplateChild<gtk::Stack>,

        /// The two handles every callback on this pane needs, as `OnceCell`s for the
        /// reason `window.rs` gives: neither is a `glib::Value`.
        pub model: OnceCell<Rc<ScannerListModel>>,
        pub commands: OnceCell<Sender<BusCommand>>,

        /// The two filtered views of the one store list. Held because `render` asks each
        /// for `n_items` — that count, and not the store's, is what decides whether a
        /// group is on screen.
        pub paired_model: OnceCell<gtk::FilterListModel>,
        pub discovered_model: OnceCell<gtk::FilterListModel>,

        /// The two panes `detail_stack` shows, which are not template classes yet
        /// ([10.21] and [10.24]). Held rather than looked up out of the stack, because
        /// `render` calls `render`/`clear` on each — methods the stack's `gtk::Widget`
        /// does not have.
        pub details: OnceCell<Rc<DetailsPane>>,
        pub buttons_page: OnceCell<Rc<ButtonsPage>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ScannersPane {
        /// Must match `template $ScannersPane` in `scanners-pane.blp`.
        const NAME: &'static str = "ScannersPane";
        type Type = super::ScannersPane;
        type ParentType = gtk::Box;

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

    impl ObjectImpl for ScannersPane {}
    impl WidgetImpl for ScannersPane {}
    impl BoxImpl for ScannersPane {}
}

glib::wrapper! {
    /// The Scanners page, built from `scanners-pane.blp` and written from the store.
    pub struct ScannersPane(ObjectSubclass<scanners_pane::ScannersPane>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl ScannersPane {
    /// Builds the pane, binds both lists to their filtered views of the store, and fills
    /// the detail stack.
    pub fn new(model: Rc<ScannerListModel>, commands: Sender<BusCommand>) -> Self {
        let pane: Self = glib::Object::new();

        // Empty cells: the object was built on the line above and this is its only
        // constructor, so no `set` below can fail — see the note in `window.rs`.
        let imp = pane.imp();
        assert!(imp.model.set(Rc::clone(&model)).is_ok(), "model set twice");
        assert!(
            imp.commands.set(commands.clone()).is_ok(),
            "commands set twice"
        );

        // The model is not shape, so it stays here rather than in the `.blp`: one
        // `ListStore` in the store, two `CustomFilter`s over it, and a `FilterListModel`
        // per list. Filtering rather than keeping two stores is what lets a scanner move
        // from Discovered to Paired without being destroyed and rebuilt.
        let paired_filter = gtk::CustomFilter::new(|item| {
            item.downcast_ref::<ScannerObject>()
                .is_some_and(ScannerObject::paired)
        });
        let discovered_filter = gtk::CustomFilter::new(|item| {
            item.downcast_ref::<ScannerObject>()
                .is_some_and(|scanner| !scanner.paired())
        });
        let paired_model = gtk::FilterListModel::new(Some(model.list()), Some(paired_filter));
        let discovered_model =
            gtk::FilterListModel::new(Some(model.list()), Some(discovered_filter));

        // Both factories are a downcast and one `ScannerRow::new`, and nothing else.
        // That is the in-place rule of §3: work done in a factory is work that runs again
        // every time the model changes, and the row — with its selection and its pairing
        // progress — is what would be thrown away to do it. These two calls are also the
        // only callers `ScannerRow::new` is meant to have.
        imp.paired_list.bind_model(Some(&paired_model), {
            let commands = commands.clone();
            let model = Rc::clone(&model);
            move |item| {
                ScannerRow::new(&scanner_item(item), commands.clone(), Rc::clone(&model)).upcast()
            }
        });
        imp.discovered_list.bind_model(Some(&discovered_model), {
            let commands = commands.clone();
            let model = Rc::clone(&model);
            move |item| {
                ScannerRow::new(&scanner_item(item), commands.clone(), Rc::clone(&model)).upcast()
            }
        });

        assert!(
            imp.paired_model.set(paired_model).is_ok(),
            "paired model set twice"
        );
        assert!(
            imp.discovered_model.set(discovered_model).is_ok(),
            "discovered model set twice"
        );

        // The two pages `scanners-pane.blp` leaves out, added by instance for the reason
        // the `.blp` gives there: neither class exists to the builder yet, and both are
        // built with the bus channel a builder-instantiated child could not be given.
        let details = Rc::new(DetailsPane::new());
        let buttons_page = Rc::new(ButtonsPage::new(commands));
        imp.detail_stack.add_named(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .min_content_width(360)
                .child(details.widget())
                .build(),
            Some("details"),
        );
        imp.detail_stack.add_named(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .min_content_width(360)
                .child(buttons_page.widget())
                .build(),
            Some("buttons"),
        );
        assert!(imp.details.set(details).is_ok(), "details set twice");
        assert!(
            imp.buttons_page.set(buttons_page).is_ok(),
            "buttons page set twice"
        );

        // The three callbacks the two sub-panes raise. Each holds a weak handle: this pane
        // owns both sub-panes through the cells just filled, and each sub-pane keeps every
        // callback it is given for its own lifetime, so a strong `self` would be a cycle
        // and the pane would never be finalised.
        {
            let weak = pane.downgrade();
            imp.details
                .get()
                .expect("details set just above")
                .connect_configure(move || {
                    let Some(pane) = weak.upgrade() else {
                        return;
                    };
                    let imp = pane.imp();
                    let model = imp.model.get().expect("model set in `new`");
                    // Configure is only reachable for a paired scanner; the check is
                    // against the store rather than against the row, because the scanner
                    // may have been unpaired since the pane was last written.
                    if model
                        .selected_path()
                        .and_then(|path| model.scanner(&path))
                        .is_some_and(|scanner| scanner.state.paired)
                    {
                        imp.detail_stack.set_visible_child_name("buttons");
                    }
                });
        }

        {
            let weak = pane.downgrade();
            imp.buttons_page
                .get()
                .expect("buttons page set just above")
                .connect_back(move || {
                    if let Some(pane) = weak.upgrade() {
                        pane.imp().detail_stack.set_visible_child_name("details");
                    }
                });
        }

        {
            let weak = pane.downgrade();
            imp.details
                .get()
                .expect("details set just above")
                .connect_unpair(move || {
                    if let Some(pane) = weak.upgrade() {
                        pane.confirm_unpair();
                    }
                });
        }

        // Registered once, here, rather than from a `#[template_callback]`: `render` is
        // driven by the store rather than by a signal on any widget of this pane. Weak
        // again, and for the same reason `settings.rs` gives — `ScannerListModel` keeps
        // every callback it is given for its own lifetime.
        let weak = pane.downgrade();
        model.connect_changed(move || {
            if let Some(pane) = weak.upgrade() {
                pane.render();
            }
        });

        // Seeds the derived visibility now the widgets exist. It is a `render` and not a
        // `ScannerListModel::refresh` because nothing about the store has changed: a
        // programmatic selection may already have been requested, and every other view of
        // it is written by its own registration.
        pane.render();

        pane
    }

    /// Writes everything on this pane that is derived from the store: the banner, both
    /// group headers, the empty state, and which detail page is up.
    ///
    /// The rows themselves are not written here — each is bound to its own
    /// `ScannerObject` and repaints itself, which is the whole of why a `Status` change
    /// does not move the selection.
    fn render(&self) {
        let imp = self.imp();
        let model = imp.model.get().expect("model set in `new`");
        let details = imp.details.get().expect("details set in `new`");
        let buttons_page = imp.buttons_page.get().expect("buttons page set in `new`");

        let paired_count = imp
            .paired_model
            .get()
            .expect("paired model set in `new`")
            .n_items();
        let discovered_count = imp
            .discovered_model
            .get()
            .expect("discovered model set in `new`")
            .n_items();
        let discovery_state = model.discovery_state();

        imp.paired_group.set_visible(paired_count > 0);
        imp.discovered_group.set_visible(discovered_count > 0);
        imp.service_banner
            .set_reveal_child(model.service_state() == ServiceState::Absent);
        imp.scanners_stack
            .set_visible_child_name(if model.has_scanners() {
                "content"
            } else {
                "empty"
            });

        if let Some(path) = model.selected_path()
            && let Some(scanner) = model.scanner(&path)
        {
            details.render(&scanner);
            buttons_page.render(&scanner, &model.profiles());
            if imp.detail_stack.visible_child_name().as_deref() == Some("placeholder") {
                imp.detail_stack.set_visible_child_name("details");
            }
            // A scanner unpaired while its buttons page was up has no buttons to
            // configure any more.
            if !scanner.state.paired
                && imp.detail_stack.visible_child_name().as_deref() == Some("buttons")
            {
                imp.detail_stack.set_visible_child_name("details");
            }
        } else {
            details.clear();
            buttons_page.clear();
            imp.detail_stack.set_visible_child_name("placeholder");
        }

        // The discovered group's description is where the "Finding scanners… N found"
        // caption is written: it is a header on the group it counts, so there is no
        // caption widget of its own.
        let caption = discovery_caption_text(discovery_state, model.discovered_count());
        imp.discovered_group.set_description(match discovery_state {
            DiscoveryState::Idle => None,
            DiscoveryState::Starting | DiscoveryState::Active | DiscoveryState::Stopping => {
                Some(caption.as_str())
            }
        });
    }

    /// The confirmation the detail pane's *Unpair* row raises.
    ///
    /// Still built by hand rather than from `unpair-dialog.blp`: its body names the
    /// scanner, and the whole dialog exists only for the length of one answer. The
    /// transient parent is looked up through `self.root()`, which is why this is a method
    /// on the pane and not a free function.
    fn confirm_unpair(&self) {
        let imp = self.imp();
        let model = imp.model.get().expect("model set in `new`");
        let commands = imp.commands.get().expect("commands set in `new`").clone();

        let Some(path) = model.selected_path() else {
            return;
        };
        let Some(scanner) = model.scanner(&path) else {
            return;
        };

        let window = gtk::Window::builder()
            .modal(true)
            .title("Unpair scanner?")
            .build();
        let body = gtk::Label::new(Some(&format!(
            "Unpairing {} removes its saved pairing information.",
            scanner.state.name
        )));
        body.set_wrap(true);
        body.set_xalign(0.0);
        body.set_margin_top(18);
        body.set_margin_bottom(18);
        body.set_margin_start(18);
        body.set_margin_end(18);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        actions.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        let destructive = gtk::Button::with_label("Unpair");
        destructive.add_css_class("destructive-action");
        actions.append(&cancel);
        actions.append(&destructive);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
        content.append(&body);
        content.append(&actions);
        window.set_child(Some(&content));
        window.set_transient_for(self.root().and_downcast_ref::<gtk::Window>());

        cancel.connect_clicked({
            let window = window.clone();
            move |_| window.close()
        });
        destructive.connect_clicked({
            let window = window.clone();
            move |_| {
                let _ = commands.try_send(BusCommand::Unpair { path: path.clone() });
                window.close();
            }
        });
        window.present();
    }
}

/// The two handlers `scanners-pane.blp` names.
///
/// Both are declared `swapped` there, which is what puts the pane in `&self`; the
/// emitting list box is the argument that gets dropped, and the pane reaches it again
/// through `self.imp()` when it needs to.
#[gtk::template_callbacks]
impl ScannersPane {
    /// Clears the discovered list's selection, then tells the store which scanner is
    /// selected.
    ///
    /// The `unselect_all` makes the mirror handler below fire with `None`, which is what
    /// the second arm guards: it reports "nothing selected" only when its *own* list has
    /// nothing selected either, so the pair of handlers does not erase the selection the
    /// other one has just made.
    #[template_callback]
    fn on_paired_selected(&self, row: Option<gtk::ListBoxRow>) {
        let imp = self.imp();
        let model = imp.model.get().expect("model set in `new`");

        if let Some(row) = row {
            imp.discovered_list.unselect_all();
            model.set_selected_path(Some(row.widget_name().to_string()));
        } else if imp.paired_list.selected_row().is_none() {
            model.set_selected_path(None);
        }
    }

    /// Mirror of [`Self::on_paired_selected`].
    #[template_callback]
    fn on_discovered_selected(&self, row: Option<gtk::ListBoxRow>) {
        let imp = self.imp();
        let model = imp.model.get().expect("model set in `new`");

        if let Some(row) = row {
            imp.paired_list.unselect_all();
            model.set_selected_path(Some(row.widget_name().to_string()));
        } else if imp.discovered_list.selected_row().is_none() {
            model.set_selected_path(None);
        }
    }
}

/// The one line of either `bind_model` factory that is not `ScannerRow::new`.
///
/// A `Gtk.FilterListModel` over the store's `ListStore` yields nothing else, so a failure
/// here is a filter pointed at the wrong model rather than anything a user could cause.
fn scanner_item(item: &glib::Object) -> ScannerObject {
    item.clone()
        .downcast::<ScannerObject>()
        .expect("the filtered model should contain ScannerObject")
}

pub fn discovery_caption_text(state: DiscoveryState, discovered_count: usize) -> String {
    match state {
        DiscoveryState::Idle => String::new(),
        DiscoveryState::Starting | DiscoveryState::Active | DiscoveryState::Stopping => {
            let noun = if discovered_count == 1 {
                "scanner"
            } else {
                "scanners"
            };
            format!("Finding scanners... {discovered_count} {noun} found")
        }
    }
}

fn pairing_state_name(state: &PairingState) -> &'static str {
    state.as_str()
}

fn pairing_error(state: &PairingState) -> String {
    state.pairing_error().to_owned()
}

fn pairing_code(state: &PairingState) -> String {
    state.pairing_info().to_owned()
}

pub(crate) fn humanize_backend(backend: &str) -> String {
    // The ids the three backends actually publish (`scanbus_backend_*::ID`, plus the
    // `proprietary:brother` form of API §3). Spelling them out is what keeps the row
    // reading "Brother" and "HPLIP" rather than "Brother Skey" and "Hplip"; anything
    // else still falls through to the generic humanisation below.
    match backend {
        "brother-skey" | "proprietary:brother" => return "Brother".to_owned(),
        "hplip" => return "HPLIP".to_owned(),
        "mobile" => return "Mobile".to_owned(),
        _ => {}
    }

    let short = backend
        .rsplit([':', '/', '.'])
        .next()
        .unwrap_or(backend)
        .replace(['-', '_'], " ");

    let mut words = Vec::new();
    for word in short.split_whitespace() {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            let mut rendered = first.to_uppercase().collect::<String>();
            rendered.push_str(chars.as_str());
            words.push(rendered);
        }
    }

    if words.is_empty() {
        backend.to_owned()
    } else {
        words.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_line_covers_the_full_cross_product() {
        let mut seen = HashSet::new();

        for status in Status::ALL {
            for connected in [false, true] {
                for paired in [false, true] {
                    let label = status_line(status, connected, paired);
                    assert!(!label.is_empty());
                    seen.insert((status, connected, paired, label));
                }
            }
        }

        assert_eq!(seen.len(), Status::ALL.len() * 2 * 2);
        assert_eq!(status_line(Status::Offline, false, true), "Offline");
        assert_eq!(
            status_line(Status::Online, false, false),
            "Discovered • Not paired"
        );
        assert_eq!(
            status_line(Status::Busy, true, false),
            "Discovered • Not paired"
        );
    }

    #[test]
    fn discovery_caption_tracks_count_while_active() {
        assert_eq!(discovery_caption_text(DiscoveryState::Idle, 2), "");
        assert_eq!(
            discovery_caption_text(DiscoveryState::Starting, 0),
            "Finding scanners... 0 scanners found"
        );
        assert_eq!(
            discovery_caption_text(DiscoveryState::Active, 1),
            "Finding scanners... 1 scanner found"
        );
    }

    #[test]
    fn connection_copy_separates_the_fact_from_the_switch() {
        assert_eq!(connection_value(false), "Disconnected");
        assert_eq!(connection_value(true), "Connected");
        assert_eq!(
            connection_subtitle(Status::Offline, false),
            "Unavailable while the scanner is offline"
        );
        assert_eq!(
            connection_subtitle(Status::Online, true),
            "This host is ready to receive from this scanner"
        );
        assert_eq!(
            connection_banner(Status::Online, false, true),
            "Paired • Online • Host not listening"
        );
    }

    #[test]
    fn default_profile_copy_is_human_readable() {
        assert_eq!(default_profile_label(None), "None");
        assert_eq!(
            default_profile_label(Some(ProfileKind::Document)),
            "Document"
        );
        // The two kinds the design does not spell as a capitalised word.
        assert_eq!(default_profile_label(Some(ProfileKind::Ocr)), "OCR");
        assert_eq!(default_profile_label(Some(ProfileKind::Email)), "E-mail");
    }

    #[test]
    fn the_summary_line_splits_where_the_colour_stops() {
        assert_eq!(
            status_summary(Status::Online, true, true),
            ("Online", " • Connected")
        );
        assert_eq!(
            status_summary(Status::Offline, false, true),
            ("Offline", "")
        );
        assert_eq!(
            status_summary(Status::Online, true, false),
            ("Discovered", " • Not paired")
        );
    }

    #[test]
    fn pairing_helpers_render_generic_confirmation_and_failure() {
        let confirmation = PairingState::AwaitingConfirmation("482913".to_owned());
        assert_eq!(pairing_state_name(&confirmation), "awaiting_confirmation");
        assert_eq!(pairing_code(&confirmation), "482913");
        assert_eq!(pairing_error(&confirmation), "");

        let failed = PairingState::Failed("backend install failed".to_owned());
        assert_eq!(pairing_state_name(&failed), "failed");
        assert_eq!(pairing_code(&failed), "");
        assert_eq!(pairing_error(&failed), "backend install failed");
    }

    #[test]
    fn backend_labels_are_humanized() {
        assert_eq!(humanize_backend("proprietary:brother"), "Brother");
        assert_eq!(humanize_backend("mock_backend"), "Mock Backend");
        // The ids the shipped backends publish, spelled as the design spells them.
        assert_eq!(humanize_backend("brother-skey"), "Brother");
        assert_eq!(humanize_backend("hplip"), "HPLIP");
        assert_eq!(humanize_backend("mobile"), "Mobile");
    }
}

/// The Scanners pane's half of the one GTK test — see [`crate::gtk_tests`].
///
/// Two instantiations, and the second is the reason the first is here: `ScannersPane`
/// fills its `#[template_child]`s from `scanners-pane.blp`, and every `ScannerRow` the
/// two `bind_model` factories build fills its own from `scanner-row.blp`. An id renamed
/// or dropped in either fails here rather than on a pane a user opens.
///
/// The store is driven with wire dicts rather than with `ScannerEntry` literals: it is
/// the same path a running daemon takes, so a decode this crate gets wrong is a failure
/// here too — the argument `options.rs` makes for its own fixtures.
#[cfg(all(test, feature = "gtk-tests"))]
pub(crate) mod widget_checks {
    use scanbus_core::{ScannerId, path};
    use zbus::zvariant::{OwnedValue, Value as ZValue};

    use super::*;
    use crate::store::{Dict, SCANNER_INTERFACE};

    fn owned(value: ZValue<'_>) -> OwnedValue {
        OwnedValue::try_from(value).expect("the fixture must be a D-Bus value")
    }

    fn scanner_id(address: &str) -> ScannerId {
        ScannerId::from_backend("mock", address).expect("a well-formed mock id")
    }

    /// A `Scanner1` property set, as `Properties.GetAll` would answer it.
    fn scanner_dict(address: &str, name: &str, paired: bool) -> Dict {
        HashMap::from([
            (
                "Id".to_owned(),
                owned(ZValue::from(scanner_id(address).as_str())),
            ),
            ("Name".to_owned(), owned(ZValue::from(name))),
            (
                "Backend".to_owned(),
                owned(ZValue::from("proprietary:brother")),
            ),
            ("Address".to_owned(), owned(ZValue::from(address))),
            (
                "Capabilities".to_owned(),
                owned(ZValue::from(HashMap::<String, OwnedValue>::new())),
            ),
            (
                "SupportedProfiles".to_owned(),
                owned(ZValue::from(vec!["document".to_owned()])),
            ),
            ("Paired".to_owned(), owned(ZValue::from(paired))),
            // Connected only where paired: an unpaired scanner has nothing to connect
            // with, and the status line says so.
            ("Connected".to_owned(), owned(ZValue::from(paired))),
            ("Status".to_owned(), owned(ZValue::from("online"))),
            ("DefaultProfile".to_owned(), owned(ZValue::from("document"))),
            ("PairingState".to_owned(), owned(ZValue::from("none"))),
            ("PairingError".to_owned(), owned(ZValue::from(""))),
            (
                "PairingInfo".to_owned(),
                owned(ZValue::from(HashMap::<String, OwnedValue>::new())),
            ),
        ])
    }

    fn snapshot(scanners: &[(&str, &str, bool)]) -> StoreEvent {
        StoreEvent::Replace(
            scanners
                .iter()
                .map(|(address, name, paired)| {
                    (
                        path::scanner(&scanner_id(address)),
                        HashMap::from([(
                            SCANNER_INTERFACE.to_owned(),
                            scanner_dict(address, name, *paired),
                        )]),
                    )
                })
                .collect(),
        )
    }

    fn row_at(list: &gtk::ListBox, index: i32) -> ScannerRow {
        list.row_at_index(index)
            .unwrap_or_else(|| panic!("no row at index {index}"))
            .downcast::<ScannerRow>()
            .expect("the factories build `ScannerRow`s, and a `Gtk.ListBox` wraps nothing else")
    }

    pub(crate) fn run() {
        let model = Rc::new(ScannerListModel::new());
        // The receiver is kept: a dropped one closes the channel, and every `try_send` in
        // this file and in `scanner_row.rs` ignores its error, so the buttons below would
        // still look as though they had sent something.
        let (commands, sent) = async_channel::unbounded();
        let pane = ScannersPane::new(Rc::clone(&model), commands);
        let imp = pane.imp();

        // A store that has answered nothing yet. `new` calls `render` once, so the empty
        // state is on screen rather than the content page the template happens to declare
        // first.
        assert_eq!(
            imp.scanners_stack.visible_child_name().unwrap_or_default(),
            "empty"
        );
        assert!(!imp.paired_group.get_visible());
        assert!(!imp.discovered_group.get_visible());
        assert_eq!(
            imp.detail_stack.visible_child_name().unwrap_or_default(),
            "placeholder"
        );
        assert!(!imp.service_banner.reveals_child());

        // The banner is the daemon's absence and nothing else: Unknown above is "not
        // asked yet", which is not something to warn about.
        model
            .apply_event(StoreEvent::ServiceState(ServiceState::Absent))
            .expect("a service state carries nothing to decode");
        assert!(imp.service_banner.reveals_child());
        model
            .apply_event(StoreEvent::ServiceState(ServiceState::Running))
            .expect("a service state carries nothing to decode");
        assert!(!imp.service_banner.reveals_child());

        // One of each, which is what the two `CustomFilter`s are for: one store list,
        // two views of it, and a group visible only where its view has something in it.
        model
            .apply_event(snapshot(&[
                ("usb:001:002", "Brother MFC-L2710DW", true),
                ("usb:001:003", "Brother DCP-1610W", false),
            ]))
            .expect("the fixture is the wire shape §3 fixes");

        assert_eq!(
            imp.scanners_stack.visible_child_name().unwrap_or_default(),
            "content"
        );
        assert!(imp.paired_group.get_visible());
        assert!(imp.discovered_group.get_visible());

        let paired_row = row_at(&imp.paired_list, 0);
        let discovered_row = row_at(&imp.discovered_list, 0);
        assert!(
            imp.paired_list.row_at_index(1).is_none()
                && imp.discovered_list.row_at_index(1).is_none(),
            "each filter should have taken exactly one of the two scanners"
        );
        assert_eq!(
            paired_row.scanner().expect("the row's scanner").name(),
            "Brother MFC-L2710DW"
        );
        assert_eq!(
            discovered_row.scanner().expect("the row's scanner").name(),
            "Brother DCP-1610W"
        );

        // Selecting through the list box, which is what a pointer does: the handler is a
        // `#[template_callback]`, so this is also the check that `swapped` is still on
        // `row-selected` in the `.blp`.
        imp.paired_list.select_row(Some(&paired_row));
        let paired_path = paired_row.scanner().expect("the row's scanner").path();
        assert_eq!(model.selected_path().as_deref(), Some(paired_path.as_str()));
        assert_eq!(
            imp.detail_stack.visible_child_name().unwrap_or_default(),
            "details",
            "a selection should leave the placeholder"
        );

        // The invariant of §3, and the one this issue could break: a `Status` change
        // repaints the row that is already there. A rebuilt row would be a different
        // widget, and the selection and the detail pane would have gone with the old one.
        model
            .apply_event(StoreEvent::PropertiesChanged {
                path: paired_path.clone(),
                interface: SCANNER_INTERFACE.to_owned(),
                changed: HashMap::from([("Status".to_owned(), owned(ZValue::from("busy")))]),
                invalidated: Vec::new(),
            })
            .expect("a `Status` of `busy` is one §3 fixes");
        assert_eq!(
            row_at(&imp.paired_list, 0),
            paired_row,
            "a status change rebuilt the row instead of repainting it"
        );
        assert_eq!(
            paired_row
                .scanner()
                .expect("the row's scanner")
                .status_line(),
            "Busy • Connected",
            "the subtitle binding should have followed the store"
        );
        assert_eq!(model.selected_path().as_deref(), Some(paired_path.as_str()));
        assert_eq!(
            imp.detail_stack.visible_child_name().unwrap_or_default(),
            "details"
        );

        // Selecting in the other list clears this one, and the guard in the handler pair
        // is what stops the `unselect_all` that does it from erasing the selection being
        // made: the store ends up with the row just clicked, not with `None`.
        imp.discovered_list.select_row(Some(&discovered_row));
        let discovered_path = discovered_row.scanner().expect("the row's scanner").path();
        assert!(imp.paired_list.selected_row().is_none());
        assert_eq!(
            model.selected_path().as_deref(),
            Some(discovered_path.as_str()),
            "the mirror handler erased the selection it was told about"
        );

        // Deselecting everything empties the detail stack rather than leaving the last
        // scanner on screen.
        imp.discovered_list.unselect_all();
        assert_eq!(model.selected_path(), None);
        assert_eq!(
            imp.detail_stack.visible_child_name().unwrap_or_default(),
            "placeholder"
        );

        // The caption is the discovered group's description, so there is no caption
        // widget to look at — this is the whole of where that string reaches the screen.
        assert_eq!(imp.discovered_group.description(), None);
        model.mark_discovery_active();
        assert_eq!(
            imp.discovered_group.description().unwrap_or_default(),
            "Finding scanners... 1 scanner found"
        );
        model.mark_discovery_idle();
        assert_eq!(imp.discovered_group.description(), None);

        // The row's own checks, on a row this pane's factory built — see the note there
        // on why they are not a `run()` of their own in `gtk_tests.rs`.
        crate::scanner_row::widget_checks::run(&discovered_row, &sent);

        // A scanner that stops being exported takes its row with it, and the selection
        // the row check just made with it.
        model
            .apply_event(snapshot(&[("usb:001:002", "Brother MFC-L2710DW", true)]))
            .expect("the fixture is the wire shape §3 fixes");
        assert!(imp.discovered_list.row_at_index(0).is_none());
        assert!(!imp.discovered_group.get_visible());
        assert_eq!(model.selected_path(), None);
        assert_eq!(
            imp.detail_stack.visible_child_name().unwrap_or_default(),
            "placeholder"
        );
    }
}
