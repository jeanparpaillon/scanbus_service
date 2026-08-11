use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gio::ListStore;
use glib::subclass::prelude::*;
use gtk::gio;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use scanbus_core::Status;

use crate::bus::BusCommand;
use crate::store::{DiscoveryState, ScannerEntry, ServiceState, Store, StoreEvent};

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
        #[property(get, set, type = bool)]
        pub paired: Cell<bool>,
        #[property(get, set, type = bool)]
        pub connected: Cell<bool>,
        #[property(get, set, type = String)]
        pub status: RefCell<String>,
        #[property(get, set, type = String)]
        pub status_line: RefCell<String>,
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
        let scanner = glib::Object::builder::<Self>()
            .property("path", &entry.path)
            .property("name", &entry.state.name)
            .property("address", &entry.state.address)
            .property("backend", &entry.state.backend)
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
            .build();
        scanner
    }

    fn update_from_entry(&self, entry: &ScannerEntry) {
        self.set_property("path", &entry.path);
        self.set_property("name", &entry.state.name);
        self.set_property("address", &entry.state.address);
        self.set_property("backend", &entry.state.backend);
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

type Callback = Box<dyn Fn() + 'static>;
type ToastCallback = Box<dyn Fn(String) + 'static>;

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
        F: Fn(String) + 'static,
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

    pub fn emit_toast(&self, message: impl Into<String>) {
        let message = message.into();
        for callback in self.toast_callbacks.borrow().iter() {
            callback(message.clone());
        }
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

        let detail_title = gtk::Label::new(Some("Scanner details"));
        detail_title.add_css_class("title-3");
        detail_title.set_xalign(0.0);

        let detail_hint = gtk::Label::new(Some(
            "Selection is exposed here for issue 10.5 to build on.",
        ));
        detail_hint.add_css_class("dim-label");
        detail_hint.set_xalign(0.0);
        detail_hint.set_wrap(true);

        let selected_path = gtk::Label::new(Some("No scanner selected"));
        selected_path.set_xalign(0.0);
        selected_path.set_wrap(true);
        selected_path.set_selectable(true);

        let detail = gtk::Box::new(gtk::Orientation::Vertical, 12);
        detail.set_margin_top(24);
        detail.set_margin_bottom(24);
        detail.set_margin_start(24);
        detail.set_margin_end(24);
        detail.set_size_request(320, -1);
        detail.append(&detail_title);
        detail.append(&detail_hint);
        detail.append(&selected_path);

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        content.set_vexpand(true);
        content.append(&scroller);
        content.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        content.append(&detail);

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
            let selected_path = selected_path.clone();
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

                selected_path.set_label(
                    &model_for_callback
                        .selected_path()
                        .unwrap_or_else(|| "No scanner selected".to_owned()),
                );

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

        // Seed the derived visibility once the widgets exist.
        model.set_selected_path(None);

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
    pair_button.set_sensitive(false);
    pair_button.set_valign(gtk::Align::Center);
    pair_button.connect_clicked({
        let path = scanner.path();
        move |_| {
            let _ = commands.try_send(BusCommand::Pair { path: path.clone() });
        }
    });

    scanner
        .bind_property("paired", &pair_button, "visible")
        .transform_to(|_, paired: bool| Some(!paired))
        .sync_create()
        .build();

    let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row_box.set_margin_top(12);
    row_box.set_margin_bottom(12);
    row_box.set_margin_start(12);
    row_box.set_margin_end(12);
    row_box.append(&text);
    row_box.append(&gtk::Box::builder().hexpand(true).build());
    row_box.append(&pair_button);

    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&row_box));
    row.set_selectable(true);
    row.set_activatable(true);
    row.set_widget_name(&scanner.path());
    row.connect_activate({
        let path = scanner.path();
        move |_| model.set_selected_path(Some(path.clone()))
    });
    row.upcast()
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
}
