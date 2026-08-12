use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::time::Duration;

use gio::prelude::*;
use gtk::gio;
use gtk::glib;
use gtk::glib::variant::ToVariant;
use gtk4 as gtk;
use libadwaita as adw;
use scanbus_client::convert;
use scanbus_core::{JobState, Value};
use zbus::zvariant::{OwnedValue, Value as ZValue};

use crate::lifecycle::AppLifecycle;
use crate::scanners::ScannerListModel;
use crate::store::{Dict, StoreEvent};

const JOB_INTERFACE: &str = "org.scanbus.Job1";
const SCANNER_INTERFACE: &str = "org.scanbus.Scanner1";
const OPEN_FILE_ACTION: &str = "app.open-file";
const OPEN_FOLDER_ACTION: &str = "app.open-folder";
const SHOW_SCANNER_ACTION: &str = "app.show-scanner";

pub struct Notifier {
    app: adw::Application,
    state: Rc<RefCell<NotificationEngine>>,
}

impl Notifier {
    pub fn new(
        app: &adw::Application,
        scanners: Rc<ScannerListModel>,
        lifecycle: Rc<AppLifecycle>,
    ) -> Self {
        install_actions(app, scanners, lifecycle);
        Self {
            app: app.clone(),
            state: Rc::new(RefCell::new(NotificationEngine::default())),
        }
    }

    pub fn handle_store_event(&self, event: &StoreEvent) {
        let effects = self.state.borrow_mut().handle_store_event(event);
        self.apply_effects(effects);
    }

    fn on_progress_timeout(&self, job_path: String) {
        let effects = self.state.borrow_mut().handle_progress_timeout(&job_path);
        self.apply_effects(effects);
    }

    fn apply_effects(&self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::ScheduleProgress { job_path } => {
                    let notifier = self.clone();
                    glib::timeout_add_local_once(Duration::from_secs(2), move || {
                        notifier.on_progress_timeout(job_path);
                    });
                }
                Effect::Send { id, notification } => {
                    self.app.send_notification(Some(&id), &notification.build());
                }
                Effect::Withdraw { id } => self.app.withdraw_notification(&id),
            }
        }
    }
}

impl Clone for Notifier {
    fn clone(&self) -> Self {
        Self {
            app: self.app.clone(),
            state: Rc::clone(&self.state),
        }
    }
}

fn install_actions(
    app: &adw::Application,
    scanners: Rc<ScannerListModel>,
    lifecycle: Rc<AppLifecycle>,
) {
    let open_file = gio::SimpleAction::new("open-file", Some(&String::static_variant_type()));
    {
        let lifecycle = Rc::clone(&lifecycle);
        open_file.connect_activate(move |_, parameter| {
            let Some(parameter) = parameter else {
                return;
            };
            let Some(path) = parameter.get::<String>() else {
                return;
            };
            launch_file(&path, &lifecycle, false);
        });
    }
    app.add_action(&open_file);

    let open_folder = gio::SimpleAction::new("open-folder", Some(&String::static_variant_type()));
    {
        let lifecycle = Rc::clone(&lifecycle);
        open_folder.connect_activate(move |_, parameter| {
            let Some(parameter) = parameter else {
                return;
            };
            let Some(path) = parameter.get::<String>() else {
                return;
            };
            launch_file(&path, &lifecycle, true);
        });
    }
    app.add_action(&open_folder);

    let show_scanner =
        gio::SimpleAction::new("show-scanner", Some(&String::static_variant_type()));
    {
        let app = app.clone();
        let scanners = Rc::clone(&scanners);
        show_scanner.connect_activate(move |_, parameter| {
            let Some(parameter) = parameter else {
                return;
            };
            let Some(path) = parameter.get::<String>() else {
                return;
            };
            scanners.set_selected_path(Some(path));
            app.activate();
        });
    }
    app.add_action(&show_scanner);
}

fn launch_file(path: &str, lifecycle: &Rc<AppLifecycle>, containing_folder: bool) {
    let file = gio::File::for_path(path);
    let launcher = gtk::FileLauncher::new(Some(&file));
    let parent = lifecycle.current_window();

    if containing_folder {
        launcher.open_containing_folder(
            parent.as_ref(),
            None::<&gio::Cancellable>,
            move |result| {
                if let Err(error) = result {
                    eprintln!("scanbus-gui: could not open containing folder: {error}");
                }
            },
        );
    } else {
        launcher.launch(parent.as_ref(), None::<&gio::Cancellable>, move |result| {
            if let Err(error) = result {
                eprintln!("scanbus-gui: could not open file: {error}");
            }
        });
    }
}

#[derive(Debug, Clone)]
enum Effect {
    ScheduleProgress { job_path: String },
    Send { id: String, notification: PreparedNotification },
    Withdraw { id: String },
}

#[derive(Debug, Clone, Default)]
struct NotificationEngine {
    scanner_names: HashMap<String, String>,
    jobs: HashMap<String, JobRecord>,
}

impl NotificationEngine {
    fn handle_store_event(&mut self, event: &StoreEvent) -> Vec<Effect> {
        match event {
            StoreEvent::ServicePresent(false) => self.handle_service_absent(),
            StoreEvent::Replace(snapshot) => {
                self.scanner_names.clear();
                self.jobs.clear();
                for (path, interfaces) in snapshot {
                    if let Some(scanner) = interfaces.get(SCANNER_INTERFACE)
                        && let Some(name) = string(scanner, "Name")
                    {
                        self.scanner_names.insert(path.clone(), name);
                    }
                }
                Vec::new()
            }
            StoreEvent::InterfacesAdded { path, interfaces } => {
                let mut effects = Vec::new();
                if let Some(scanner) = interfaces.get(SCANNER_INTERFACE)
                    && let Some(name) = string(scanner, "Name")
                {
                    self.scanner_names.insert(path.clone(), name);
                }
                if let Some(properties) = interfaces.get(JOB_INTERFACE)
                    && let Some(record) = JobRecord::from_properties(properties)
                {
                    self.jobs.insert(path.clone(), record);
                    effects.push(Effect::ScheduleProgress {
                        job_path: path.clone(),
                    });
                }
                effects
            }
            StoreEvent::InterfacesRemoved { path, interfaces } => {
                let mut effects = Vec::new();
                if interfaces.iter().any(|name| name == SCANNER_INTERFACE) {
                    self.scanner_names.remove(path);
                }
                if interfaces.iter().any(|name| name == JOB_INTERFACE)
                    && let Some(record) = self.jobs.remove(path)
                    && record.progress_visible
                    && !record.terminal_seen
                {
                    effects.push(Effect::Withdraw {
                        id: notification_id(path),
                    });
                }
                effects
            }
            StoreEvent::PropertiesChanged {
                path,
                interface,
                changed,
                invalidated: _,
            } => {
                if interface == SCANNER_INTERFACE {
                    if let Some(name) = changed.get("Name").and_then(|value| string_value(value)) {
                        self.scanner_names.insert(path.clone(), name);
                    }
                    return Vec::new();
                }

                if interface != JOB_INTERFACE {
                    return Vec::new();
                }

                let Some(record) = self.jobs.get_mut(path) else {
                    return Vec::new();
                };
                record.apply(changed);

                if let Some(state) = record.terminal_state_from(changed) {
                    record.terminal_seen = true;
                    let notification = match state {
                        JobState::Done => {
                            let result = changed
                                .get("Result")
                                .and_then(dict_value)
                                .and_then(|dict| convert::from_dict(&dict).ok())
                                .unwrap_or_default();
                            success_notification(
                                self.scanner_names
                                    .get(&record.scanner_path)
                                    .cloned()
                                    .unwrap_or_else(|| record.scanner_path.clone()),
                                &record.scanner_path,
                                result,
                            )
                        }
                        JobState::Error(message) => PreparedNotification::error(
                            self.scanner_names
                                .get(&record.scanner_path)
                                .cloned()
                                .unwrap_or_else(|| record.scanner_path.clone()),
                            message,
                            record.scanner_path.clone(),
                        ),
                        JobState::Receiving | JobState::Processing => unreachable!(),
                    };
                    return vec![Effect::Send {
                        id: notification_id(path),
                        notification,
                    }];
                }

                if record.should_show_progress() {
                    record.progress_visible = true;
                    return vec![Effect::Send {
                        id: notification_id(path),
                        notification: PreparedNotification::progress(
                            self.scanner_names
                                .get(&record.scanner_path)
                                .cloned()
                                .unwrap_or_else(|| record.scanner_path.clone()),
                            record.page_count,
                        ),
                    }];
                }

                Vec::new()
            }
            StoreEvent::ServicePresent(true) => Vec::new(),
        }
    }

    fn handle_progress_timeout(&mut self, job_path: &str) -> Vec<Effect> {
        let Some(record) = self.jobs.get_mut(job_path) else {
            return Vec::new();
        };
        if record.terminal_seen || record.progress_visible {
            return Vec::new();
        }

        record.progress_visible = true;
        vec![Effect::Send {
            id: notification_id(job_path),
            notification: PreparedNotification::progress(
                self.scanner_names
                    .get(&record.scanner_path)
                    .cloned()
                    .unwrap_or_else(|| record.scanner_path.clone()),
                record.page_count,
            ),
        }]
    }

    fn handle_service_absent(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        for (job_path, record) in self.jobs.drain() {
            if record.progress_visible && !record.terminal_seen {
                effects.push(Effect::Withdraw {
                    id: notification_id(&job_path),
                });
            }
        }
        self.scanner_names.clear();
        effects
    }
}

#[derive(Debug, Clone)]
struct JobRecord {
    scanner_path: String,
    state: String,
    page_count: u32,
    progress_visible: bool,
    terminal_seen: bool,
}

impl JobRecord {
    fn from_properties(properties: &Dict) -> Option<Self> {
        Some(Self {
            scanner_path: string(properties, "Scanner")?,
            state: string(properties, "State")?,
            page_count: unsigned(properties, "PageCount")?,
            progress_visible: false,
            terminal_seen: false,
        })
    }

    fn apply(&mut self, changed: &Dict) {
        if let Some(scanner_path) = changed.get("Scanner").and_then(string_value) {
            self.scanner_path = scanner_path;
        }
        if let Some(state) = changed.get("State").and_then(string_value) {
            self.state = state;
        }
        if let Some(page_count) = changed.get("PageCount").and_then(unsigned_value) {
            self.page_count = page_count;
        }
    }

    fn should_show_progress(&self) -> bool {
        !self.terminal_seen && !self.progress_visible && self.page_count > 1
    }

    fn terminal_state_from(&self, changed: &Dict) -> Option<JobState> {
        let state = changed.get("State").and_then(string_value)?;
        match state.as_str() {
            "done" => Some(JobState::Done),
            "error" => Some(JobState::Error(
                changed
                    .get("Error")
                    .and_then(string_value)
                    .unwrap_or_default(),
            )),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct PreparedNotification {
    title: String,
    body: String,
    priority: gio::NotificationPriority,
    buttons: Vec<NotificationButton>,
}

impl PreparedNotification {
    fn progress(title: String, page_count: u32) -> Self {
        Self {
            title,
            body: progress_body(page_count),
            priority: gio::NotificationPriority::Low,
            buttons: Vec::new(),
        }
    }

    fn error(title: String, body: String, scanner_path: String) -> Self {
        Self {
            title,
            body,
            priority: gio::NotificationPriority::Urgent,
            buttons: vec![NotificationButton {
                label: "Details".to_owned(),
                action: SHOW_SCANNER_ACTION.to_owned(),
                target: scanner_path,
            }],
        }
    }

    fn build(&self) -> gio::Notification {
        let notification = gio::Notification::new(&self.title);
        notification.set_body(Some(&self.body));
        notification.set_priority(self.priority);
        for button in &self.buttons {
            notification.add_button_with_target_value(
                &button.label,
                &button.action,
                Some(&button.target.to_variant()),
            );
        }
        notification
    }
}

#[derive(Debug, Clone)]
struct NotificationButton {
    label: String,
    action: String,
    target: String,
}

fn success_notification(
    title: String,
    scanner_path: &str,
    result: BTreeMap<String, Value>,
) -> PreparedNotification {
    if let Some(path) = result.get("path").and_then(Value::as_str) {
        let mut body = String::from("Scan saved");
        if let Some(preview) = result.get("text_preview").and_then(Value::as_str)
            && !preview.is_empty()
        {
            body = preview.to_owned();
        }
        return PreparedNotification {
            title,
            body,
            priority: gio::NotificationPriority::Normal,
            buttons: open_buttons(path),
        };
    }

    if let Some(paths) = result.get("paths").and_then(Value::as_array) {
        let files: Vec<&str> = paths.iter().filter_map(Value::as_str).collect();
        if let Some(first) = files.first() {
            let body = if files.len() == 1 {
                "Scan saved".to_owned()
            } else {
                format!("{} files", files.len())
            };
            return PreparedNotification {
                title,
                body,
                priority: gio::NotificationPriority::Normal,
                buttons: open_buttons(first),
            };
        }
    }

    if let Some(client) = result.get("client").and_then(Value::as_str) {
        let draft_created = result
            .get("draft_created")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        return PreparedNotification {
            title,
            body: if draft_created {
                format!("Email draft created in {client}")
            } else {
                format!("Email result reported by {client}")
            },
            priority: gio::NotificationPriority::Normal,
            buttons: Vec::new(),
        };
    }

    PreparedNotification {
        title,
        body: format!(
            "Scan completed, but this client does not recognize the result shape from {scanner_path}"
        ),
        priority: gio::NotificationPriority::Normal,
        buttons: Vec::new(),
    }
}

fn open_buttons(path: &str) -> Vec<NotificationButton> {
    vec![
        NotificationButton {
            label: "Open".to_owned(),
            action: OPEN_FILE_ACTION.to_owned(),
            target: path.to_owned(),
        },
        NotificationButton {
            label: "Open folder".to_owned(),
            action: OPEN_FOLDER_ACTION.to_owned(),
            target: path.to_owned(),
        },
    ]
}

fn progress_body(page_count: u32) -> String {
    if page_count == 1 {
        "Scanning… 1 page received".to_owned()
    } else {
        format!("Scanning… {page_count} pages received")
    }
}

fn notification_id(job_path: &str) -> String {
    job_path.to_owned()
}

fn string(properties: &Dict, key: &str) -> Option<String> {
    properties.get(key).and_then(string_value)
}

fn unsigned(properties: &Dict, key: &str) -> Option<u32> {
    properties.get(key).and_then(unsigned_value)
}

fn string_value(value: &OwnedValue) -> Option<String> {
    match Into::<ZValue<'_>>::into(value.try_clone().ok()?) {
        ZValue::Str(text) => Some(text.as_str().to_owned()),
        ZValue::ObjectPath(path) => Some(path.as_str().to_owned()),
        _ => None,
    }
}

fn unsigned_value(value: &OwnedValue) -> Option<u32> {
    convert::from_variant(&Into::<ZValue<'_>>::into(value.try_clone().ok()?))
        .ok()?
        .as_u64()
        .and_then(|number| u32::try_from(number).ok())
}

fn dict_value(value: &OwnedValue) -> Option<Dict> {
    value.try_clone().ok()?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn owned(value: ZValue<'_>) -> OwnedValue {
        OwnedValue::try_from(value).unwrap()
    }

    fn scanner_path() -> String {
        "/org/scanbus/scanner/mock_usb_001_002".to_owned()
    }

    fn job_path() -> String {
        format!("{}/job/1", scanner_path())
    }

    fn job_added(page_count: u32) -> StoreEvent {
        StoreEvent::InterfacesAdded {
            path: job_path(),
            interfaces: HashMap::from([(
                JOB_INTERFACE.to_owned(),
                HashMap::from([
                    ("Scanner".to_owned(), owned(ZValue::from(scanner_path().as_str()))),
                    ("Button".to_owned(), owned(ZValue::from(0i32))),
                    ("Profile".to_owned(), owned(ZValue::from("document"))),
                    ("State".to_owned(), owned(ZValue::from("receiving"))),
                    ("PageCount".to_owned(), owned(ZValue::from(page_count))),
                    (
                        "Result".to_owned(),
                        owned(ZValue::from(HashMap::<String, OwnedValue>::new())),
                    ),
                    ("Error".to_owned(), owned(ZValue::from(""))),
                ]),
            )]),
        }
    }

    fn scanner_added(name: &str) -> StoreEvent {
        StoreEvent::InterfacesAdded {
            path: scanner_path(),
            interfaces: HashMap::from([(
                SCANNER_INTERFACE.to_owned(),
                HashMap::from([("Name".to_owned(), owned(ZValue::from(name)))]),
            )]),
        }
    }

    #[test]
    fn one_page_scan_skips_progress_and_posts_result() {
        let mut engine = NotificationEngine::default();
        engine.handle_store_event(&scanner_added("Brother"));
        let effects = engine.handle_store_event(&job_added(1));
        assert!(matches!(
            effects.as_slice(),
            [Effect::ScheduleProgress { job_path: scheduled }] if scheduled == &job_path()
        ));

        let done = engine.handle_store_event(&StoreEvent::PropertiesChanged {
            path: job_path(),
            interface: JOB_INTERFACE.to_owned(),
            changed: HashMap::from([
                ("State".to_owned(), owned(ZValue::from("done"))),
                (
                    "Result".to_owned(),
                    owned(ZValue::from(HashMap::from([(
                        "path".to_owned(),
                        owned(ZValue::from("/tmp/scan.pdf")),
                    )]))),
                ),
            ]),
            invalidated: Vec::new(),
        });

        assert!(matches!(
            done.as_slice(),
            [Effect::Send { id, notification }]
                if id == &job_path()
                && notification.title == "Brother"
                && notification.body == "Scan saved"
                && notification.buttons.len() == 2
        ));

        let timeout = engine.handle_progress_timeout(&job_path());
        assert!(timeout.is_empty());
    }

    #[test]
    fn multi_page_scan_replaces_progress_with_result() {
        let mut engine = NotificationEngine::default();
        engine.handle_store_event(&scanner_added("ADF"));
        engine.handle_store_event(&job_added(1));

        let progress = engine.handle_store_event(&StoreEvent::PropertiesChanged {
            path: job_path(),
            interface: JOB_INTERFACE.to_owned(),
            changed: HashMap::from([("PageCount".to_owned(), owned(ZValue::from(3u32)))]),
            invalidated: Vec::new(),
        });
        assert!(matches!(
            progress.as_slice(),
            [Effect::Send { notification, .. }] if notification.body == "Scanning… 3 pages received"
        ));

        let done = engine.handle_store_event(&StoreEvent::PropertiesChanged {
            path: job_path(),
            interface: JOB_INTERFACE.to_owned(),
            changed: HashMap::from([
                ("State".to_owned(), owned(ZValue::from("done"))),
                (
                    "Result".to_owned(),
                    owned(ZValue::from(HashMap::from([(
                        "paths".to_owned(),
                        owned(ZValue::from(vec![
                            owned(ZValue::from("/tmp/a.jpg")),
                            owned(ZValue::from("/tmp/b.jpg")),
                            owned(ZValue::from("/tmp/c.jpg")),
                        ])),
                    )]))),
                ),
            ]),
            invalidated: Vec::new(),
        });
        assert!(matches!(
            done.as_slice(),
            [Effect::Send { notification, .. }] if notification.body == "3 files"
        ));
    }

    #[test]
    fn daemon_vanishing_withdraws_only_progress_notification() {
        let mut engine = NotificationEngine::default();
        engine.handle_store_event(&scanner_added("Brother"));
        engine.handle_store_event(&job_added(2));

        let _ = engine.handle_store_event(&StoreEvent::PropertiesChanged {
            path: job_path(),
            interface: JOB_INTERFACE.to_owned(),
            changed: HashMap::from([("PageCount".to_owned(), owned(ZValue::from(2u32)))]),
            invalidated: Vec::new(),
        });

        let effects = engine.handle_store_event(&StoreEvent::ServicePresent(false));
        assert!(matches!(
            effects.as_slice(),
            [Effect::Withdraw { id }] if id == &job_path()
        ));
    }

    #[test]
    fn error_notification_carries_details_action() {
        let mut engine = NotificationEngine::default();
        engine.handle_store_event(&scanner_added("Brother"));
        engine.handle_store_event(&job_added(1));

        let effects = engine.handle_store_event(&StoreEvent::PropertiesChanged {
            path: job_path(),
            interface: JOB_INTERFACE.to_owned(),
            changed: HashMap::from([
                ("State".to_owned(), owned(ZValue::from("error"))),
                (
                    "Error".to_owned(),
                    owned(ZValue::from("the scanner stopped answering mid-transfer")),
                ),
            ]),
            invalidated: Vec::new(),
        });

        assert!(matches!(
            effects.as_slice(),
            [Effect::Send { notification, .. }]
                if notification.priority == gio::NotificationPriority::Urgent
                && notification.body == "the scanner stopped answering mid-transfer"
                && notification.buttons[0].action == SHOW_SCANNER_ACTION
        ));
    }

    #[test]
    fn scanner_path_is_the_fallback_title_once_the_scanner_is_gone() {
        let mut engine = NotificationEngine::default();
        engine.handle_store_event(&job_added(1));
        let effects = engine.handle_store_event(&StoreEvent::PropertiesChanged {
            path: job_path(),
            interface: JOB_INTERFACE.to_owned(),
            changed: HashMap::from([
                ("State".to_owned(), owned(ZValue::from("done"))),
                (
                    "Result".to_owned(),
                    owned(ZValue::from(HashMap::from([(
                        "path".to_owned(),
                        owned(ZValue::from("/tmp/scan.pdf")),
                    )]))),
                ),
            ]),
            invalidated: Vec::new(),
        });

        assert!(matches!(
            effects.as_slice(),
            [Effect::Send { notification, .. }] if notification.title == scanner_path()
        ));
    }
}
