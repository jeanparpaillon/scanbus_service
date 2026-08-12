use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use gio::ListStore;
use glib::subclass::prelude::*;
use gtk::gio;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use scanbus_core::{PairingState, ProfileKind, Status};

use crate::bus::BusCommand;
use crate::buttons::ButtonsPage;
use crate::store::{DiscoveryState, ProfileEntry, ScannerEntry, ServiceState, Store, StoreEvent};

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

pub fn status_line(status: Status, connected: bool, paired: bool) -> String {
    if !paired {
        return "Discovered • Not paired".to_owned();
    }

    match status {
        Status::Offline => "Offline".to_owned(),
        Status::Online => {
            if connected {
                "Online • Connected".to_owned()
            } else {
                "Online".to_owned()
            }
        }
        Status::Busy => {
            if connected {
                "Busy • Connected".to_owned()
            } else {
                "Busy".to_owned()
            }
        }
        Status::Error => {
            if connected {
                "Error • Connected".to_owned()
            } else {
                "Error".to_owned()
            }
        }
    }
}

fn default_profile_label(profile: Option<ProfileKind>) -> String {
    profile.map_or_else(
        || "None".to_owned(),
        |profile| humanize_profile(profile.as_str()),
    )
}

pub fn humanize_profile(profile: &str) -> String {
    let mut chars = profile.chars();
    if let Some(first) = chars.next() {
        let mut label = first.to_uppercase().collect::<String>();
        label.push_str(chars.as_str());
        label
    } else {
        "None".to_owned()
    }
}

fn status_value(status: Status) -> &'static str {
    match status {
        Status::Offline => "Offline",
        Status::Online => "Online",
        Status::Busy => "Busy",
        Status::Error => "Error",
    }
}

fn connection_value(connected: bool) -> &'static str {
    if connected {
        "Listening"
    } else {
        "Not listening"
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

pub struct ScannersPane {
    root: gtk::Box,
}

impl ScannersPane {
    pub fn new(model: Rc<ScannerListModel>, commands: async_channel::Sender<BusCommand>) -> Self {
        let service_banner = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .reveal_child(false)
            .build();
        let service_banner_body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        service_banner_body.add_css_class("toolbar");
        service_banner_body.set_margin_top(12);
        service_banner_body.set_margin_bottom(12);
        service_banner_body.set_margin_start(18);
        service_banner_body.set_margin_end(18);
        let service_banner_label = gtk::Label::new(Some("Scanbus service is not running"));
        service_banner_label.set_xalign(0.0);
        service_banner_body.append(&service_banner_label);
        service_banner.set_child(Some(&service_banner_body));

        let empty_state = adw::StatusPage::builder()
            .title("No scanners yet")
            .description("Find scanners…")
            .build();
        empty_state.set_vexpand(true);

        let paired_filter = gtk::CustomFilter::new(|item| {
            item.downcast_ref::<ScannerObject>()
                .is_some_and(ScannerObject::paired)
        });
        let discovered_filter = gtk::CustomFilter::new(|item| {
            item.downcast_ref::<ScannerObject>()
                .is_some_and(|scanner| !scanner.paired())
        });

        let paired_model =
            gtk::FilterListModel::new(Some(model.list()), Some(paired_filter.clone()));
        let discovered_model =
            gtk::FilterListModel::new(Some(model.list()), Some(discovered_filter.clone()));

        let paired_group = adw::PreferencesGroup::builder()
            .title("Paired scanners")
            .build();
        let paired_list = gtk::ListBox::new();
        paired_list.add_css_class("boxed-list");
        paired_list.set_selection_mode(gtk::SelectionMode::Single);
        paired_list.bind_model(Some(&paired_model), {
            let commands = commands.clone();
            let model = Rc::clone(&model);
            move |item| scanner_row(item, commands.clone(), Rc::clone(&model))
        });
        paired_group.add(&paired_list);

        let discovered_group = adw::PreferencesGroup::builder()
            .title("Discovered scanners")
            .build();
        let discovery_caption = gtk::Label::new(None);
        discovery_caption.add_css_class("dim-label");
        discovery_caption.set_xalign(0.0);
        let discovered_list = gtk::ListBox::new();
        discovered_list.add_css_class("boxed-list");
        discovered_list.set_selection_mode(gtk::SelectionMode::Single);
        discovered_list.bind_model(Some(&discovered_model), {
            let commands = commands.clone();
            let model = Rc::clone(&model);
            move |item| scanner_row(item, commands.clone(), Rc::clone(&model))
        });
        discovered_group.add(&discovered_list);

        let lists = gtk::Box::new(gtk::Orientation::Vertical, 18);
        lists.set_margin_top(24);
        lists.set_margin_bottom(24);
        lists.set_margin_start(24);
        lists.set_margin_end(24);
        lists.append(&paired_group);
        lists.append(&discovered_group);

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .min_content_width(420)
            .child(&lists)
            .build();
        scroller.set_hexpand(true);
        scroller.set_vexpand(true);

        let detail_placeholder = adw::StatusPage::builder()
            .title("No scanner selected")
            .description("Select a scanner to inspect it")
            .build();

        let detail_title = gtk::Label::new(Some("Scanner details"));
        detail_title.add_css_class("title-3");
        detail_title.set_xalign(0.0);

        let detail_name = gtk::Label::new(None);
        detail_name.add_css_class("heading");
        detail_name.set_xalign(0.0);
        detail_name.set_wrap(true);

        let detail_status_value = gtk::Label::new(None);
        detail_status_value.set_xalign(0.0);
        detail_status_value.set_wrap(true);

        let detail_connection_value = gtk::Label::new(None);
        detail_connection_value.set_xalign(0.0);
        detail_connection_value.set_wrap(true);

        let detail_connection_hint = gtk::Label::new(None);
        detail_connection_hint.add_css_class("dim-label");
        detail_connection_hint.set_xalign(0.0);
        detail_connection_hint.set_wrap(true);

        let detail_address_value = gtk::Label::new(None);
        detail_address_value.set_xalign(0.0);
        detail_address_value.set_wrap(true);
        detail_address_value.set_selectable(true);

        let detail_backend_value = gtk::Label::new(None);
        detail_backend_value.set_xalign(0.0);
        detail_backend_value.set_wrap(true);

        let detail_default_profile_value = gtk::Label::new(None);
        detail_default_profile_value.set_xalign(0.0);
        detail_default_profile_value.set_wrap(true);

        let configure_buttons = gtk::Button::with_label("Configure buttons");
        configure_buttons.set_halign(gtk::Align::Start);
        configure_buttons.add_css_class("flat");
        configure_buttons.set_visible(false);

        let unpair_button = gtk::Button::with_label("Unpair");
        unpair_button.add_css_class("destructive-action");
        unpair_button.set_halign(gtk::Align::Start);
        unpair_button.set_visible(false);

        let detail_page = gtk::Box::new(gtk::Orientation::Vertical, 18);
        detail_page.set_margin_top(24);
        detail_page.set_margin_bottom(24);
        detail_page.set_margin_start(24);
        detail_page.set_margin_end(24);
        detail_page.append(&detail_title);
        detail_page.append(&detail_name);
        detail_page.append(&detail_fact_row("Status", &detail_status_value));
        detail_page.append(&detail_fact_with_hint_row(
            "Connection",
            &detail_connection_value,
            &detail_connection_hint,
        ));
        detail_page.append(&detail_fact_row("Address", &detail_address_value));
        detail_page.append(&detail_fact_row("Backend", &detail_backend_value));
        detail_page.append(&detail_fact_row(
            "Default profile",
            &detail_default_profile_value,
        ));
        detail_page.append(&configure_buttons);
        detail_page.append(&unpair_button);

        let detail_scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .min_content_width(360)
            .child(&detail_page)
            .build();

        let buttons_page = Rc::new(ButtonsPage::new(commands.clone()));

        let buttons_scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .min_content_width(360)
            .child(buttons_page.widget())
            .build();

        let detail_stack = gtk::Stack::new();
        detail_stack.add_named(&detail_placeholder, Some("placeholder"));
        detail_stack.add_named(&detail_scroller, Some("details"));
        detail_stack.add_named(&buttons_scroller, Some("buttons"));
        detail_stack.set_visible_child_name("placeholder");

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        content.set_vexpand(true);
        content.append(&scroller);
        content.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        content.append(&detail_stack);

        let stack = gtk::Stack::new();
        stack.add_named(&content, Some("content"));
        stack.add_named(&empty_state, Some("empty"));
        stack.set_vexpand(true);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&service_banner);
        root.append(&stack);
        root.set_hexpand(true);
        root.set_vexpand(true);

        {
            let model = Rc::clone(&model);
            let discovered_list = discovered_list.clone();
            paired_list.connect_row_selected(move |list, row| {
                if let Some(row) = row {
                    discovered_list.unselect_all();
                    model.set_selected_path(Some(row.widget_name().to_string()));
                } else if list.selected_row().is_none() {
                    model.set_selected_path(None);
                }
            });
        }

        {
            let model = Rc::clone(&model);
            let paired_list = paired_list.clone();
            discovered_list.connect_row_selected(move |list, row| {
                if let Some(row) = row {
                    paired_list.unselect_all();
                    model.set_selected_path(Some(row.widget_name().to_string()));
                } else if list.selected_row().is_none() {
                    model.set_selected_path(None);
                }
            });
        }

        {
            let model_for_callback = Rc::clone(&model);
            let stack = stack.clone();
            let service_banner = service_banner.clone();
            let paired_group = paired_group.clone();
            let discovered_group = discovered_group.clone();
            let discovery_caption = discovery_caption.clone();
            let detail_stack = detail_stack.clone();
            let detail_name = detail_name.clone();
            let detail_status_value = detail_status_value.clone();
            let detail_connection_value = detail_connection_value.clone();
            let detail_connection_hint = detail_connection_hint.clone();
            let detail_address_value = detail_address_value.clone();
            let detail_backend_value = detail_backend_value.clone();
            let detail_default_profile_value = detail_default_profile_value.clone();
            let configure_buttons = configure_buttons.clone();
            let buttons_page = Rc::clone(&buttons_page);
            let unpair_button = unpair_button.clone();
            model.connect_changed(move || {
                let paired_count = paired_model.n_items();
                let discovered_count = discovered_model.n_items();
                let discovery_state = model_for_callback.discovery_state();

                paired_group.set_visible(paired_count > 0);
                discovered_group.set_visible(discovered_count > 0);
                service_banner
                    .set_reveal_child(model_for_callback.service_state() == ServiceState::Absent);
                stack.set_visible_child_name(if model_for_callback.has_scanners() {
                    "content"
                } else {
                    "empty"
                });

                if let Some(path) = model_for_callback.selected_path()
                    && let Some(scanner) = model_for_callback.scanner(&path)
                {
                    detail_name.set_label(&scanner.state.name);
                    detail_status_value.set_label(status_value(scanner.state.status));
                    detail_connection_value.set_label(connection_value(scanner.state.connected));
                    detail_connection_hint.set_label(connection_subtitle(
                        scanner.state.status,
                        scanner.state.connected,
                    ));
                    detail_address_value.set_label(&scanner.state.address);
                    detail_backend_value.set_label(&humanize_backend(&scanner.state.backend));
                    detail_default_profile_value
                        .set_label(&default_profile_label(scanner.state.default_profile));
                    configure_buttons.set_visible(scanner.state.paired);
                    configure_buttons.set_sensitive(scanner.state.paired);
                    buttons_page.render(&scanner, &model_for_callback.profiles());
                    unpair_button.set_visible(scanner.state.paired);
                    unpair_button.set_sensitive(scanner.state.paired);
                    if detail_stack.visible_child_name().as_deref() == Some("placeholder") {
                        detail_stack.set_visible_child_name("details");
                    }
                    if !scanner.state.paired
                        && detail_stack.visible_child_name().as_deref() == Some("buttons")
                    {
                        detail_stack.set_visible_child_name("details");
                    }
                } else {
                    detail_name.set_label("");
                    detail_status_value.set_label("");
                    detail_connection_value.set_label("");
                    detail_connection_hint.set_label("");
                    detail_address_value.set_label("");
                    detail_backend_value.set_label("");
                    detail_default_profile_value.set_label("");
                    configure_buttons.set_visible(false);
                    buttons_page.clear();
                    unpair_button.set_visible(false);
                    unpair_button.set_sensitive(false);
                    detail_stack.set_visible_child_name("placeholder");
                }

                let caption =
                    discovery_caption_text(discovery_state, model_for_callback.discovered_count());
                discovery_caption.set_label(&caption);
                discovered_group.set_description(match discovery_state {
                    DiscoveryState::Idle => None,
                    DiscoveryState::Starting
                    | DiscoveryState::Active
                    | DiscoveryState::Stopping => Some(caption.as_str()),
                });
            });
        }

        // Seed the derived visibility once the widgets exist without erasing a
        // programmatic selection that may already have been requested.
        model.refresh();

        {
            let detail_stack = detail_stack.clone();
            let model = Rc::clone(&model);
            configure_buttons.connect_clicked(move |_| {
                if model
                    .selected_path()
                    .and_then(|path| model.scanner(&path))
                    .is_some_and(|scanner| scanner.state.paired)
                {
                    detail_stack.set_visible_child_name("buttons");
                }
            });
        }

        {
            let detail_stack = detail_stack.clone();
            buttons_page.connect_back(move || {
                detail_stack.set_visible_child_name("details");
            });
        }

        {
            let model = Rc::clone(&model);
            let commands = commands.clone();
            let parent = root.clone();
            unpair_button.connect_clicked(move |_| {
                let Some(path) = model.selected_path() else {
                    return;
                };
                let Some(scanner) = model.scanner(&path) else {
                    return;
                };

                let dialog = gtk::Dialog::builder()
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
                dialog.content_area().append(&body);
                dialog.add_button("Cancel", gtk::ResponseType::Cancel);
                let destructive = dialog.add_button("Unpair", gtk::ResponseType::Accept);
                destructive.add_css_class("destructive-action");
                dialog.connect_response({
                    let commands = commands.clone();
                    move |dialog, response| {
                        if response == gtk::ResponseType::Accept {
                            let _ = commands.try_send(BusCommand::Unpair { path: path.clone() });
                        }
                        dialog.close();
                    }
                });
                dialog.set_transient_for(parent.root().and_downcast_ref::<gtk::Window>());
                dialog.present();
            });
        }

        Self { root }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }
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

fn detail_fact_row(title: &str, value: &gtk::Label) -> gtk::Box {
    let title_label = gtk::Label::new(Some(title));
    title_label.add_css_class("caption-heading");
    title_label.set_xalign(0.0);

    let row = gtk::Box::new(gtk::Orientation::Vertical, 4);
    row.append(&title_label);
    row.append(value);
    row
}

fn detail_fact_with_hint_row(title: &str, value: &gtk::Label, hint: &gtk::Label) -> gtk::Box {
    let row = detail_fact_row(title, value);
    row.append(hint);
    row
}

fn scanner_row(
    item: &glib::Object,
    commands: async_channel::Sender<BusCommand>,
    model: Rc<ScannerListModel>,
) -> gtk::Widget {
    let scanner = item
        .clone()
        .downcast::<ScannerObject>()
        .expect("preferences group model should contain ScannerObject");

    let title = gtk::Label::new(None);
    title.set_xalign(0.0);
    title.add_css_class("heading");
    scanner
        .bind_property("name", &title, "label")
        .sync_create()
        .build();

    let subtitle = gtk::Label::new(None);
    subtitle.set_xalign(0.0);
    subtitle.add_css_class("dim-label");
    scanner
        .bind_property("status-line", &subtitle, "label")
        .sync_create()
        .build();

    let text = gtk::Box::new(gtk::Orientation::Vertical, 4);
    text.append(&title);
    text.append(&subtitle);

    let pair_button = gtk::Button::with_label("Pair");
    pair_button.set_valign(gtk::Align::Center);

    let spinner = gtk::Spinner::new();
    spinner.set_halign(gtk::Align::Start);

    let progress_label = gtk::Label::new(None);
    progress_label.set_xalign(0.0);
    progress_label.add_css_class("dim-label");
    progress_label.set_wrap(true);

    let progress_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    progress_box.append(&spinner);
    progress_box.append(&progress_label);

    let confirm_label = gtk::Label::new(Some("Confirm on your device"));
    confirm_label.set_xalign(0.0);

    let code_label = gtk::Label::new(None);
    code_label.add_css_class("title-2");
    code_label.add_css_class("monospace");
    code_label.set_xalign(0.0);

    let confirm_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    confirm_box.append(&confirm_label);
    confirm_box.append(&code_label);

    let failure_bar = gtk::InfoBar::new();
    failure_bar.set_message_type(gtk::MessageType::Error);
    failure_bar.set_show_close_button(false);
    let failure_message = gtk::Label::new(None);
    failure_message.set_wrap(true);
    failure_message.set_xalign(0.0);
    failure_bar.add_child(&failure_message);

    let failure_details_label = gtk::Label::new(None);
    failure_details_label.set_selectable(true);
    failure_details_label.set_wrap(true);
    failure_details_label.set_xalign(0.0);

    let failure_details = gtk::Expander::new(Some("Details"));
    failure_details.set_child(Some(&failure_details_label));
    failure_details.set_expanded(false);

    let failure_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    failure_box.append(&failure_bar);
    failure_box.append(&failure_details);

    let state_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    state_box.append(&progress_box);
    state_box.append(&confirm_box);
    state_box.append(&failure_box);
    pair_button.connect_clicked({
        let scanner = scanner.clone();
        move |_| {
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
    });

    let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row_box.append(&text);
    row_box.append(&gtk::Box::builder().hexpand(true).build());
    row_box.append(&pair_button);

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 8);
    outer.set_margin_top(12);
    outer.set_margin_bottom(12);
    outer.set_margin_start(12);
    outer.set_margin_end(12);
    outer.append(&row_box);
    outer.append(&state_box);

    let update_row = {
        let scanner = scanner.clone();
        let pair_button = pair_button.clone();
        let spinner = spinner.clone();
        let progress_box = progress_box.clone();
        let progress_label = progress_label.clone();
        let confirm_box = confirm_box.clone();
        let code_label = code_label.clone();
        let failure_box = failure_box.clone();
        let failure_bar = failure_bar.clone();
        let failure_message = failure_message.clone();
        let failure_details_label = failure_details_label.clone();
        move || {
            let paired = scanner.paired();
            let pairing = scanner.pairing_state();
            let code = scanner.pairing_code();
            let error = scanner.pairing_error();

            pair_button.set_visible(!paired);
            pair_button.set_sensitive(!paired || pairing == "failed");

            progress_box.set_visible(false);
            confirm_box.set_visible(false);
            failure_box.set_visible(false);
            spinner.set_visible(false);
            spinner.set_spinning(false);
            failure_bar.set_visible(false);

            match pairing.as_str() {
                "pairing" => {
                    pair_button.set_label("Cancel");
                    progress_box.set_visible(true);
                    spinner.set_visible(true);
                    spinner.set_spinning(true);
                    progress_label.set_label("Pairing in progress");
                }
                "installing_backend" => {
                    pair_button.set_label("Cancel");
                    progress_box.set_visible(true);
                    spinner.set_visible(true);
                    spinner.set_spinning(true);
                    progress_label.set_label(&format!(
                        "Installing {} backend…",
                        humanize_backend(&scanner.backend())
                    ));
                }
                "awaiting_confirmation" => {
                    pair_button.set_label("Cancel");
                    confirm_box.set_visible(true);
                    code_label.set_label(&code);
                }
                "failed" => {
                    pair_button.set_label("Try again");
                    failure_box.set_visible(true);
                    failure_bar.set_visible(true);
                    failure_message.set_label("Pairing failed");
                    failure_details_label.set_label(&error);
                }
                _ => {
                    pair_button.set_label("Pair");
                }
            }
        }
    };

    update_row();
    scanner.connect_notify_local(None, move |_, _| update_row());

    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&outer));
    row.set_selectable(true);
    row.set_activatable(true);
    row.set_widget_name(&scanner.path());
    row.connect_activate({
        let path = scanner.path();
        move |_| model.set_selected_path(Some(path.clone()))
    });
    row.upcast()
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

fn humanize_backend(backend: &str) -> String {
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
    fn connection_copy_spells_out_host_listening() {
        assert_eq!(connection_value(false), "Not listening");
        assert_eq!(connection_value(true), "Listening");
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
    }
}
