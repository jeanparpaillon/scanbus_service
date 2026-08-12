//! The *Configure buttons* page — `docs/design/buttons.png`.
//!
//! Two decisions here come straight out of the API rather than out of the mockup, and
//! both are about refusing to offer something:
//!
//! - `LabelConfigurable=false` is the common case (API §5's Brother MFC has four fixed
//!   keys), and API §9 says the property exists to "prevent a UI client from offering a
//!   rename action that would silently fail". So a fixed label is a [`gtk::Label`], not
//!   an insensitive entry — an insensitive entry is that same offer with a grey tint.
//! - `DeviceLabel` and `Profile` are *allowed* to diverge, and API §5 makes warning about
//!   it the client's job. The row therefore says so in a caption and does nothing else:
//!   no dialog, no refusal, no "fix it" button. The user may well mean it, and on the
//!   device that produces the mismatch most often the label cannot be changed anyway.
//!
//! Every write is a property set, and the store stays the truth: nothing here shows the
//! value the user picked until the daemon has confirmed it through `PropertiesChanged`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use async_channel::Sender;
use gtk::gio;
use gtk::glib::variant::{StaticVariantType, ToVariant};
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use scanbus_client::convert;
use scanbus_core::{ProfileKind, Status, Value, implied_profile, path};

use crate::bus::BusCommand;
use crate::scanners::{connection_banner, connection_subtitle, humanize_profile};
use crate::store::{ButtonEntry, ProfileEntry, ScannerEntry};

const PROFILE_ACTION: &str = "profile";
const DEFAULT_PROFILE_ACTION: &str = "default-profile";

/// What one row needs to know when a callback fires, refreshed on every render.
///
/// The button's object path is not in here: a row is built for one `(scanner, index)`
/// and rebuilt when either changes, so the path is fixed for its lifetime and the
/// callbacks hold it directly. Keeping it out also keeps every render a single
/// `borrow_mut` with nothing borrowed alongside it.
#[derive(Debug, Default, Clone)]
struct RowState {
    label: String,
    profile: Option<ProfileKind>,
    overrides: BTreeMap<String, Value>,
    profile_options: BTreeMap<String, Value>,
}

/// The same, for the *Default profile* group and the connection switch.
#[derive(Debug, Default, Clone)]
struct PageState {
    scanner_path: String,
    paired: bool,
    status: Status,
    default_profile: Option<ProfileKind>,
    profile_options: BTreeMap<String, Value>,
}

struct ButtonRow {
    index: u32,
    root: gtk::ListBoxRow,
    label_text: gtk::Label,
    label_entry: gtk::Entry,
    caption: gtk::Label,
    profile_button: gtk::MenuButton,
    profile_action: gio::SimpleAction,
    menu_for: RefCell<Vec<ProfileKind>>,
    summary: gtk::Label,
    configure: gtk::Button,
    state: Rc<RefCell<RowState>>,
    /// A label write is in flight, so the next render owns the entry's text again.
    pending_label: Rc<RefCell<bool>>,
}

pub struct ButtonsPage {
    root: gtk::Box,
    back: gtk::Button,
    banner_title: gtk::Label,
    banner_address: gtk::Label,
    connection_hint: gtk::Label,
    connection_switch: gtk::Switch,
    group: adw::PreferencesGroup,
    list: gtk::ListBox,
    empty: gtk::Label,
    default_button: gtk::MenuButton,
    default_action: gio::SimpleAction,
    default_menu_for: RefCell<Vec<ProfileKind>>,
    default_summary: gtk::Label,
    default_configure: gtk::Button,
    rows: RefCell<Vec<ButtonRow>>,
    /// The `(scanner path, button indices)` the rows were built for.
    binding: RefCell<Option<(String, Vec<u32>)>>,
    state: Rc<RefCell<PageState>>,
    commands: Sender<BusCommand>,
}

impl ButtonsPage {
    pub fn new(commands: Sender<BusCommand>) -> Self {
        let back = gtk::Button::with_label("Back");
        back.add_css_class("flat");

        let title = gtk::Label::new(Some("Configure buttons"));
        title.add_css_class("title-3");
        title.set_xalign(0.0);
        title.set_hexpand(true);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        header.append(&back);
        header.append(&title);

        let banner_title = gtk::Label::new(None);
        banner_title.add_css_class("heading");
        banner_title.set_xalign(0.0);
        banner_title.set_wrap(true);

        let banner_address = gtk::Label::new(None);
        banner_address.add_css_class("dim-label");
        banner_address.set_xalign(0.0);
        banner_address.set_wrap(true);

        let connection_title = gtk::Label::new(Some("Connection"));
        connection_title.add_css_class("heading");
        connection_title.set_xalign(0.0);

        let connection_hint = gtk::Label::new(None);
        connection_hint.add_css_class("dim-label");
        connection_hint.set_xalign(0.0);
        connection_hint.set_wrap(true);

        let connection_text = gtk::Box::new(gtk::Orientation::Vertical, 4);
        connection_text.append(&connection_title);
        connection_text.append(&connection_hint);

        let connection_switch = gtk::Switch::new();
        connection_switch.set_halign(gtk::Align::End);
        connection_switch.set_valign(gtk::Align::Center);

        let connection_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        connection_row.append(&connection_text);
        connection_row.append(&gtk::Box::builder().hexpand(true).build());
        connection_row.append(&connection_switch);

        let banner = gtk::Box::new(gtk::Orientation::Vertical, 12);
        banner.add_css_class("card");
        banner.set_margin_top(6);
        banner.set_margin_bottom(6);
        banner.set_margin_start(6);
        banner.set_margin_end(6);
        banner.append(&banner_title);
        banner.append(&banner_address);
        banner.append(&connection_row);

        let group = adw::PreferencesGroup::builder()
            .title("Scanner buttons")
            .description(
                "Each physical button on your scanner can start a profile without \
                 opening your computer.",
            )
            .build();
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::None);
        group.add(&list);

        let empty = gtk::Label::new(None);
        empty.add_css_class("dim-label");
        empty.set_wrap(true);
        empty.set_xalign(0.0);
        empty.set_visible(false);

        let default_group = adw::PreferencesGroup::builder()
            .title("Default profile")
            .description("Profile applied by default to data received without an explicit profile.")
            .build();

        let default_action = gio::SimpleAction::new_stateful(
            DEFAULT_PROFILE_ACTION,
            Some(&String::static_variant_type()),
            &String::new().to_variant(),
        );
        let default_button = gtk::MenuButton::new();
        default_button.set_valign(gtk::Align::Center);
        let default_summary = gtk::Label::new(None);
        default_summary.add_css_class("dim-label");
        default_summary.set_xalign(1.0);
        default_summary.set_wrap(true);
        let default_configure = gtk::Button::with_label("Configure");
        default_configure.add_css_class("flat");
        default_configure.set_valign(gtk::Align::Center);

        let default_row_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        default_row_box.set_margin_top(12);
        default_row_box.set_margin_bottom(12);
        default_row_box.set_margin_start(12);
        default_row_box.set_margin_end(12);
        default_row_box.append(&default_button);
        default_row_box.append(&gtk::Box::builder().hexpand(true).build());
        default_row_box.append(&default_summary);
        default_row_box.append(&default_configure);

        let default_row = gtk::ListBoxRow::new();
        default_row.set_selectable(false);
        default_row.set_activatable(false);
        default_row.set_child(Some(&default_row_box));

        let default_list = gtk::ListBox::new();
        default_list.add_css_class("boxed-list");
        default_list.set_selection_mode(gtk::SelectionMode::None);
        default_list.append(&default_row);
        default_group.add(&default_list);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 18);
        root.set_margin_top(24);
        root.set_margin_bottom(24);
        root.set_margin_start(24);
        root.set_margin_end(24);
        root.append(&header);
        root.append(&banner);
        root.append(&group);
        root.append(&empty);
        root.append(&default_group);

        let actions = gio::SimpleActionGroup::new();
        actions.add_action(&default_action);
        root.insert_action_group("scanner", Some(&actions));

        let state = Rc::new(RefCell::new(PageState::default()));

        {
            let state = Rc::clone(&state);
            let commands = commands.clone();
            default_action.connect_activate(move |_, parameter| {
                let Some(chosen) = parameter.and_then(|value| value.get::<String>()) else {
                    return;
                };
                let Ok(kind) = ProfileKind::parse_optional(&chosen) else {
                    return;
                };
                let state = state.borrow();
                if kind == state.default_profile || state.scanner_path.is_empty() {
                    return;
                }
                let _ = commands.try_send(BusCommand::SetDefaultProfile {
                    path: state.scanner_path.clone(),
                    kind,
                });
            });
        }

        {
            let state = Rc::clone(&state);
            let commands = commands.clone();
            connection_switch.connect_state_set(move |_, active| {
                let state = state.borrow();
                if !state.paired || state.status == Status::Offline || state.scanner_path.is_empty()
                {
                    return gtk::glib::Propagation::Stop;
                }
                let path = state.scanner_path.clone();
                let _ = commands.try_send(if active {
                    BusCommand::Connect { path }
                } else {
                    BusCommand::Disconnect { path }
                });
                gtk::glib::Propagation::Stop
            });
        }

        {
            let state = Rc::clone(&state);
            let parent = root.clone();
            default_configure.connect_clicked(move |_| {
                let state = state.borrow();
                let Some(profile) = state.default_profile else {
                    return;
                };
                open_profile_options(&parent, profile, &state.profile_options, &BTreeMap::new());
            });
        }

        Self {
            root,
            back,
            banner_title,
            banner_address,
            connection_hint,
            connection_switch,
            group,
            list,
            empty,
            default_button,
            default_action,
            default_menu_for: RefCell::new(Vec::new()),
            default_summary,
            default_configure,
            rows: RefCell::new(Vec::new()),
            binding: RefCell::new(None),
            state,
            commands,
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn connect_back<F>(&self, callback: F)
    where
        F: Fn() + 'static,
    {
        self.back.connect_clicked(move |_| callback());
    }

    /// Renders the page from the store. Called for every store change, so it updates the
    /// existing widgets in place and only rebuilds rows when the scanner — or the set of
    /// keys it publishes — actually changed.
    pub fn render(&self, scanner: &ScannerEntry, profiles: &BTreeMap<ProfileKind, ProfileEntry>) {
        let state = &scanner.state;
        let supported: Vec<ProfileKind> = state.supported_profiles.clone();

        self.banner_title.set_label(&connection_banner(
            state.status,
            state.connected,
            state.paired,
        ));
        self.banner_address
            .set_label(&format!("Address: {}", state.address));
        self.connection_hint
            .set_label(connection_subtitle(state.status, state.connected));
        self.connection_switch.set_active(state.connected);
        self.connection_switch
            .set_sensitive(state.paired && state.status != Status::Offline);

        *self.state.borrow_mut() = PageState {
            scanner_path: scanner.path.clone(),
            paired: state.paired,
            status: state.status,
            default_profile: state.default_profile,
            profile_options: options_of(profiles, state.default_profile),
        };

        self.render_default_profile(state.default_profile, &supported, profiles);

        let indices: Vec<u32> = scanner.buttons.keys().copied().collect();
        self.rebind(&scanner.path, &indices, state.id.clone());

        for (row, entry) in self.rows.borrow().iter().zip(scanner.buttons.values()) {
            row.render(entry, &supported, profiles);
        }

        let has_rows = !indices.is_empty();
        self.group.set_visible(has_rows);
        self.empty.set_visible(!has_rows);
        if !has_rows {
            self.empty
                .set_label(empty_state_text(state.capabilities.buttons.count));
        }
    }

    /// Drops every row. The page keeps its widgets so the stack can hold on to it.
    pub fn clear(&self) {
        self.rebind_to_nothing();
        *self.state.borrow_mut() = PageState::default();
        self.banner_title.set_label("");
        self.banner_address.set_label("");
        self.connection_hint.set_label("");
        self.connection_switch.set_active(false);
        self.connection_switch.set_sensitive(false);
        self.group.set_visible(false);
        self.empty.set_visible(false);
    }

    fn render_default_profile(
        &self,
        default_profile: Option<ProfileKind>,
        supported: &[ProfileKind],
        profiles: &BTreeMap<ProfileKind, ProfileEntry>,
    ) {
        if *self.default_menu_for.borrow() != supported {
            self.default_button.set_menu_model(Some(&profile_menu(
                "scanner",
                DEFAULT_PROFILE_ACTION,
                supported,
            )));
            *self.default_menu_for.borrow_mut() = supported.to_vec();
        }

        self.default_action
            .set_state(&ProfileKind::optional_as_str(default_profile).to_variant());
        style_profile_button(&self.default_button, default_profile);

        let options = options_of(profiles, default_profile);
        self.default_summary.set_label(&summarise(&options));
        self.default_summary.set_visible(!options.is_empty());
        self.default_configure
            .set_sensitive(default_profile.is_some());
    }

    fn rebind(&self, scanner_path: &str, indices: &[u32], id: scanbus_core::ScannerId) {
        let wanted = (scanner_path.to_owned(), indices.to_vec());
        if self.binding.borrow().as_ref() == Some(&wanted) {
            return;
        }

        self.rebind_to_nothing();

        let mut rows = Vec::with_capacity(indices.len());
        for index in indices {
            let row = ButtonRow::new(path::button(&id, *index), *index, self.commands.clone());
            self.list.append(&row.root);
            rows.push(row);
        }
        *self.rows.borrow_mut() = rows;
        *self.binding.borrow_mut() = Some(wanted);
    }

    fn rebind_to_nothing(&self) {
        for row in self.rows.borrow().iter() {
            self.list.remove(&row.root);
        }
        self.rows.borrow_mut().clear();
        *self.binding.borrow_mut() = None;
    }
}

impl ButtonRow {
    fn new(path: String, index: u32, commands: Sender<BusCommand>) -> Self {
        let path = Rc::new(path);
        let state = Rc::new(RefCell::new(RowState::default()));
        let pending_label = Rc::new(RefCell::new(false));

        let index_label = gtk::Label::new(Some(&index.to_string()));
        index_label.add_css_class("caption-heading");
        index_label.add_css_class("numeric");
        index_label.set_width_chars(2);
        index_label.set_valign(gtk::Align::Center);
        index_label.set_tooltip_text(Some(&format!(
            "Key {index} — the index the device and `scanbus button` use"
        )));

        let label_text = gtk::Label::new(None);
        label_text.set_xalign(0.0);
        label_text.set_wrap(true);

        let label_entry = gtk::Entry::new();
        label_entry.set_visible(false);
        label_entry.set_tooltip_text(Some("Press Enter to send this label to the device"));

        let caption = gtk::Label::new(None);
        caption.add_css_class("dim-label");
        caption.add_css_class("caption");
        caption.set_xalign(0.0);
        caption.set_wrap(true);
        caption.set_visible(false);

        let label_area = gtk::Box::new(gtk::Orientation::Vertical, 4);
        label_area.set_hexpand(true);
        label_area.set_valign(gtk::Align::Center);
        label_area.append(&label_text);
        label_area.append(&label_entry);
        label_area.append(&caption);

        let profile_action = gio::SimpleAction::new_stateful(
            PROFILE_ACTION,
            Some(&String::static_variant_type()),
            &String::new().to_variant(),
        );
        let profile_button = gtk::MenuButton::new();
        profile_button.set_valign(gtk::Align::Center);

        let summary = gtk::Label::new(None);
        summary.add_css_class("dim-label");
        summary.set_xalign(1.0);
        summary.set_wrap(true);

        let configure = gtk::Button::with_label("Configure");
        configure.add_css_class("flat");
        configure.set_valign(gtk::Align::Center);

        let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row_box.set_margin_top(12);
        row_box.set_margin_bottom(12);
        row_box.set_margin_start(12);
        row_box.set_margin_end(12);
        row_box.append(&index_label);
        row_box.append(&label_area);
        row_box.append(&profile_button);
        row_box.append(&summary);
        row_box.append(&configure);

        let root = gtk::ListBoxRow::new();
        root.set_selectable(false);
        root.set_activatable(false);
        root.set_child(Some(&row_box));

        let actions = gio::SimpleActionGroup::new();
        actions.add_action(&profile_action);
        root.insert_action_group("button", Some(&actions));

        {
            let state = Rc::clone(&state);
            let path = Rc::clone(&path);
            let commands = commands.clone();
            profile_action.connect_activate(move |_, parameter| {
                let Some(chosen) = parameter.and_then(|value| value.get::<String>()) else {
                    return;
                };
                let Ok(kind) = ProfileKind::parse_optional(&chosen) else {
                    return;
                };
                if kind == state.borrow().profile {
                    return;
                }
                // No optimistic update: the action keeps the stored value, so a refused
                // write leaves the row showing the profile that is still assigned.
                let _ = commands.try_send(BusCommand::SetButtonProfile {
                    path: path.to_string(),
                    kind,
                });
            });
        }

        {
            let state = Rc::clone(&state);
            let path = Rc::clone(&path);
            let pending_label = Rc::clone(&pending_label);
            let commands = commands.clone();
            label_entry.connect_activate(move |entry| {
                let label = entry.text().to_string();
                if label == state.borrow().label {
                    return;
                }
                *pending_label.borrow_mut() = true;
                let _ = commands.try_send(BusCommand::SetButtonLabel {
                    path: path.to_string(),
                    label,
                });
            });
        }

        {
            let state = Rc::clone(&state);
            let parent = root.clone();
            configure.connect_clicked(move |_| {
                let state = state.borrow();
                let Some(profile) = state.profile else {
                    return;
                };
                open_profile_options(&parent, profile, &state.profile_options, &state.overrides);
            });
        }

        Self {
            index,
            root,
            label_text,
            label_entry,
            caption,
            profile_button,
            profile_action,
            menu_for: RefCell::new(Vec::new()),
            summary,
            configure,
            state,
            pending_label,
        }
    }

    fn render(
        &self,
        entry: &ButtonEntry,
        supported: &[ProfileKind],
        profiles: &BTreeMap<ProfileKind, ProfileEntry>,
    ) {
        debug_assert_eq!(self.index, entry.index);

        let profile_options = options_of(profiles, entry.profile);
        let overrides = decode(&entry.profile_options);

        // §5: with LabelConfigurable=false, `Label` mirrors `DeviceLabel`; the label the
        // firmware imposes is the one to show either way.
        let shown = if entry.label.is_empty() {
            entry.device_label.clone()
        } else {
            entry.label.clone()
        };

        self.label_text.set_visible(!entry.label_configurable);
        self.label_entry.set_visible(entry.label_configurable);
        self.label_text.set_label(&if shown.is_empty() {
            format!("Key {}", entry.index)
        } else {
            shown.clone()
        });

        let pending = std::mem::replace(&mut *self.pending_label.borrow_mut(), false);
        if pending || !self.label_entry.has_focus() {
            self.label_entry.set_text(&shown);
        }

        if *self.menu_for.borrow() != supported {
            self.profile_button.set_menu_model(Some(&profile_menu(
                "button",
                PROFILE_ACTION,
                supported,
            )));
            *self.menu_for.borrow_mut() = supported.to_vec();
        }
        self.profile_action
            .set_state(&ProfileKind::optional_as_str(entry.profile).to_variant());
        style_profile_button(&self.profile_button, entry.profile);

        match divergence_caption(&entry.device_label, entry.profile) {
            Some(text) => {
                self.caption.set_label(&text);
                self.caption.set_visible(true);
            }
            None => {
                self.caption.set_label("");
                self.caption.set_visible(false);
            }
        }

        let summary = summarise(&overriding(&overrides, &profile_options));
        self.summary.set_label(&summary);
        self.summary.set_visible(!summary.is_empty());
        self.configure.set_sensitive(entry.profile.is_some());

        *self.state.borrow_mut() = RowState {
            label: shown,
            profile: entry.profile,
            overrides,
            profile_options,
        };
    }
}

/// The picker of the checklist: only `Scanner1.SupportedProfiles`, plus the way back to
/// unassigned. A profile the daemon does not advertise is not offered, so
/// `UnsupportedProfile` is something the daemon says about a race, not about a menu item.
fn profile_menu(group: &str, action: &str, supported: &[ProfileKind]) -> gio::Menu {
    let menu = gio::Menu::new();
    let action = format!("{group}.{action}");

    for kind in supported {
        let item = gio::MenuItem::new(Some(&humanize_profile(kind.as_str())), None);
        item.set_action_and_target_value(Some(&action), Some(&kind.as_str().to_variant()));
        menu.append_item(&item);
    }

    let item = gio::MenuItem::new(Some("Unassigned"), None);
    item.set_action_and_target_value(Some(&action), Some(&String::new().to_variant()));
    menu.append_item(&item);

    menu
}

fn style_profile_button(button: &gtk::MenuButton, profile: Option<ProfileKind>) {
    match profile {
        Some(kind) => {
            button.set_label(&humanize_profile(kind.as_str()));
            button.remove_css_class("suggested-action");
            button.add_css_class("flat");
        }
        None => {
            button.set_label("Assign profile");
            button.remove_css_class("flat");
            button.add_css_class("suggested-action");
        }
    }
}

/// The whole of the API's divergence warning: one caption, no verdict.
pub fn divergence_caption(device_label: &str, profile: Option<ProfileKind>) -> Option<String> {
    let profile = profile?;
    let implied = implied_profile(device_label)?;
    if implied == profile {
        return None;
    }

    Some(format!(
        "This key is labelled \"{device_label}\" but runs the {profile} profile"
    ))
}

/// The keys this button overrides, and only those.
///
/// The daemon merges the two dictionaries — profile options first, button options on top
/// (`resolve_options` in `scanbus-daemon/src/profiles.rs`) — so a button option equal to
/// the profile's own changes nothing and saying it would be noise.
pub fn overriding(
    button_options: &BTreeMap<String, Value>,
    profile_options: &BTreeMap<String, Value>,
) -> BTreeMap<String, Value> {
    button_options
        .iter()
        .filter(|(key, value)| profile_options.get(*key) != Some(*value))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

pub fn summarise(options: &BTreeMap<String, Value>) -> String {
    options
        .iter()
        .map(|(key, value)| format!("{}: {}", humanize_key(key), render_value(value)))
        .collect::<Vec<_>>()
        .join(" · ")
}

pub fn humanize_key(key: &str) -> String {
    let spaced = key.replace(['_', '-'], " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

pub fn render_value(value: &Value) -> String {
    match value {
        Value::Bool(value) => (if *value { "Yes" } else { "No" }).to_owned(),
        Value::U64(value) => value.to_string(),
        Value::I64(value) => value.to_string(),
        Value::F64(value) => value.to_string(),
        Value::Str(value) => shorten_path(value),
        Value::Array(items) => items
            .iter()
            .map(render_value)
            .collect::<Vec<_>>()
            .join(", "),
        Value::Dict(_) => "…".to_owned(),
    }
}

/// `/home/jean/Documents/Scans` reads as `Documents/Scans`, as in the mockup. Anything
/// that is not under `$HOME` is shown verbatim — an absolute path elsewhere is exactly
/// the detail the user needs.
fn shorten_path(value: &str) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return value.to_owned();
    };
    let home = home.to_string_lossy().into_owned();
    if home.is_empty() {
        return value.to_owned();
    }

    value
        .strip_prefix(&format!("{}/", home.trim_end_matches('/')))
        .map_or_else(|| value.to_owned(), str::to_owned)
}

/// What a scanner with no `Button1` objects says instead of showing an empty group.
///
/// `Capabilities.buttons.count` is what the objects are created from at pairing time
/// (§5), so it separates the two reasons the group can be empty: a device that has no
/// physical menu at all — a phone, a plain eSCL scanner — and one whose keys have simply
/// not been published yet.
pub fn empty_state_text(count: u32) -> &'static str {
    if count == 0 {
        "This scanner has no keys. Its physical menu is empty, so nothing here can be \
         assigned — scans it sends arrive without a key, and the default profile below \
         is what runs for them."
    } else {
        "This scanner reports keys, but the daemon has not published them yet. They \
         appear here on their own once it does."
    }
}

fn options_of(
    profiles: &BTreeMap<ProfileKind, ProfileEntry>,
    profile: Option<ProfileKind>,
) -> BTreeMap<String, Value> {
    profile
        .and_then(|kind| profiles.get(&kind))
        .map(|entry| decode(&entry.options))
        .unwrap_or_default()
}

fn decode(dict: &crate::store::Dict) -> BTreeMap<String, Value> {
    convert::from_dict(dict).unwrap_or_default()
}

/// The options the assigned profile will run with — [10.7] owns editing them, and until
/// it lands this shows what they currently are rather than pretending there is nothing
/// there.
///
/// [10.7]: https://github.com/jeanparpaillon/scanbus_service/issues/54
fn open_profile_options(
    parent: &impl IsA<gtk::Widget>,
    profile: ProfileKind,
    profile_options: &BTreeMap<String, Value>,
    overrides: &BTreeMap<String, Value>,
) {
    let window = gtk::Window::builder()
        .modal(true)
        .title(format!("{} options", humanize_profile(profile.as_str())))
        .build();

    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    body.set_margin_top(18);
    body.set_margin_bottom(18);
    body.set_margin_start(18);
    body.set_margin_end(18);

    let mut resolved = profile_options.clone();
    for (key, value) in overrides {
        resolved.insert(key.clone(), value.clone());
    }

    if resolved.is_empty() {
        let empty = gtk::Label::new(Some("This profile has no options set."));
        empty.set_xalign(0.0);
        empty.set_wrap(true);
        body.append(&empty);
    } else {
        for (key, value) in &resolved {
            let name = gtk::Label::new(Some(&humanize_key(key)));
            name.add_css_class("caption-heading");
            name.set_xalign(0.0);

            let rendered = if overrides.contains_key(key) {
                format!("{} (set on this key)", render_value(value))
            } else {
                render_value(value)
            };
            let shown = gtk::Label::new(Some(&rendered));
            shown.set_xalign(0.0);
            shown.set_wrap(true);
            shown.set_selectable(true);

            let entry = gtk::Box::new(gtk::Orientation::Vertical, 2);
            entry.append(&name);
            entry.append(&shown);
            body.append(&entry);
        }
    }

    let note = gtk::Label::new(Some(
        "Editing these arrives with the Profiles view, which needs an option schema the \
         daemon does not publish yet.",
    ));
    note.add_css_class("dim-label");
    note.set_xalign(0.0);
    note.set_wrap(true);
    body.append(&note);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    actions.set_halign(gtk::Align::End);
    let close = gtk::Button::with_label("Close");
    actions.append(&close);
    body.append(&actions);

    window.set_child(Some(&body));
    window.set_transient_for(parent.as_ref().root().and_downcast_ref::<gtk::Window>());
    close.connect_clicked({
        let window = window.clone();
        move |_| window.close()
    });
    window.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(pairs: &[(&str, &str)]) -> BTreeMap<String, Value> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), Value::Str((*value).to_owned())))
            .collect()
    }

    /// The example the issue names: `document` on the key the firmware calls
    /// "Scan to OCR" is legal, and the caption is the entire response to it.
    #[test]
    fn a_diverging_key_gets_a_caption_and_nothing_else() {
        assert_eq!(
            divergence_caption("Scan to OCR", Some(ProfileKind::Document)).as_deref(),
            Some("This key is labelled \"Scan to OCR\" but runs the document profile")
        );
    }

    /// No opinion, agreement, and no assignment each mean silence — the heuristic must
    /// never turn into a nag.
    #[test]
    fn agreement_and_no_opinion_are_both_silent() {
        assert_eq!(
            divergence_caption("Scan to OCR", Some(ProfileKind::Ocr)),
            None
        );
        assert_eq!(
            divergence_caption("Shortcut 1", Some(ProfileKind::Image)),
            None
        );
        assert_eq!(divergence_caption("", Some(ProfileKind::Image)), None);
        assert_eq!(divergence_caption("Scan to OCR", None), None);
    }

    /// The mockup's two summaries — `Output folder: Documents/Scans` and `Format: PNG` —
    /// are per-key overrides, and a button option equal to the profile's own is not one.
    #[test]
    fn the_summary_names_only_what_the_key_overrides() {
        let profile = options(&[("output_folder", "/srv/scans"), ("format", "pdf")]);
        let button = options(&[("output_folder", "/srv/scans"), ("format", "png")]);

        let diff = overriding(&button, &profile);
        assert_eq!(diff.keys().collect::<Vec<_>>(), vec!["format"]);
        assert_eq!(summarise(&diff), "Format: png");
    }

    /// A key the profile has no value for at all is an override too.
    #[test]
    fn a_key_the_profile_does_not_set_is_still_an_override() {
        let diff = overriding(&options(&[("quality", "90")]), &BTreeMap::new());
        assert_eq!(summarise(&diff), "Quality: 90");
        assert_eq!(summarise(&BTreeMap::new()), "");
    }

    #[test]
    fn option_keys_read_as_prose() {
        assert_eq!(humanize_key("output_folder"), "Output folder");
        assert_eq!(humanize_key("format"), "Format");
        assert_eq!(humanize_key("multi-page"), "Multi page");
        assert_eq!(humanize_key(""), "");
    }

    #[test]
    fn values_render_for_a_reader_not_for_a_parser() {
        assert_eq!(render_value(&Value::Bool(true)), "Yes");
        assert_eq!(render_value(&Value::U64(300)), "300");
        assert_eq!(
            render_value(&Value::Array(vec![
                Value::Str("eng".to_owned()),
                Value::Str("fra".to_owned()),
            ])),
            "eng, fra"
        );
    }

    #[test]
    fn a_path_under_home_loses_the_home_prefix() {
        let previous = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", "/home/scanbus");
        }

        assert_eq!(
            shorten_path("/home/scanbus/Documents/Scans"),
            "Documents/Scans"
        );
        assert_eq!(shorten_path("/srv/scans"), "/srv/scans");
        assert_eq!(shorten_path("/home/scanbus"), "/home/scanbus");

        match previous {
            Some(home) => unsafe { std::env::set_var("HOME", home) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// A `buttons.count` of 0 — a phone, a plain eSCL device — is a different sentence
    /// from keys that have not arrived yet, and neither is an empty group.
    #[test]
    fn the_two_empty_states_say_different_things() {
        assert!(empty_state_text(0).contains("no keys"));
        assert!(empty_state_text(4).contains("not published them yet"));
        assert_ne!(empty_state_text(0), empty_state_text(4));
    }

    #[cfg(feature = "gtk-tests")]
    mod widgets {
        use super::*;

        use scanbus_client::ScannerState;
        use scanbus_core::{ButtonsCapability, Capabilities, PairingState, ScannerId, Status};

        use crate::store::{ButtonEntry, Dict};

        fn dict(pairs: &[(&str, &str)]) -> Dict {
            convert::to_dict(&options(pairs))
        }

        fn scanner_id() -> ScannerId {
            ScannerId::from_backend("mock", "usb:001:002").unwrap()
        }

        /// The Brother MFC of API §5: four keys, none of them renameable.
        fn brother(count: u32, label_configurable: bool) -> ScannerEntry {
            ScannerEntry {
                path: scanbus_core::path::scanner(&scanner_id()),
                state: ScannerState {
                    id: scanner_id(),
                    name: "Brother MFC-L2710DW".to_owned(),
                    backend: "proprietary:brother".to_owned(),
                    address: "192.168.1.23".to_owned(),
                    capabilities: Capabilities {
                        buttons: ButtonsCapability {
                            count,
                            label_configurable,
                        },
                        ..Capabilities::default()
                    },
                    supported_profiles: vec![ProfileKind::Image, ProfileKind::Document],
                    paired: true,
                    connected: true,
                    status: Status::Online,
                    default_profile: Some(ProfileKind::Document),
                    pairing: PairingState::Done,
                },
                buttons: BTreeMap::new(),
                jobs: BTreeMap::new(),
            }
        }

        fn button(
            index: u32,
            device_label: &str,
            label_configurable: bool,
            profile: Option<ProfileKind>,
            overrides: &[(&str, &str)],
        ) -> ButtonEntry {
            ButtonEntry {
                index,
                device_label: device_label.to_owned(),
                label_configurable,
                label: device_label.to_owned(),
                profile,
                profile_options: dict(overrides),
            }
        }

        fn profiles() -> BTreeMap<ProfileKind, ProfileEntry> {
            BTreeMap::from([
                (
                    ProfileKind::Document,
                    ProfileEntry {
                        kind: ProfileKind::Document,
                        options: dict(&[("output_folder", "/srv/scans")]),
                    },
                ),
                (
                    ProfileKind::Image,
                    ProfileEntry {
                        kind: ProfileKind::Image,
                        options: dict(&[("format", "jpeg")]),
                    },
                ),
            ])
        }

        fn visible(widget: &impl IsA<gtk::Widget>) -> bool {
            widget.as_ref().property::<bool>("visible")
        }

        /// One test, not six: GTK pins itself to the thread that initialises it, and
        /// cargo runs test functions on threads of its own. Everything the widgets have
        /// to get right is asserted here, in order.
        ///
        /// Skipped rather than failed with no display — the same deal the daemon's bus
        /// tests make with `dbus-daemon`: a headless runner should not turn the suite
        /// red over a dependency that is not a build dependency.
        #[test]
        fn the_page_renders_what_the_firmware_and_the_api_allow() {
            if libadwaita::init().is_err() {
                eprintln!("skipping: no display for GTK");
                return;
            }

            let (commands, sent) = async_channel::unbounded();
            let page = ButtonsPage::new(commands);
            let profiles = profiles();

            // --- A Brother-shaped scanner: four fixed keys, one of them diverging. ---
            let mut scanner = brother(4, false);
            scanner.buttons = BTreeMap::from([
                (
                    0,
                    button(0, "Scan to File", false, Some(ProfileKind::Document), &[]),
                ),
                (
                    1,
                    button(
                        1,
                        "Scan to Image",
                        false,
                        Some(ProfileKind::Image),
                        &[("format", "png")],
                    ),
                ),
                (
                    2,
                    button(2, "Scan to OCR", false, Some(ProfileKind::Document), &[]),
                ),
                (3, button(3, "Scan to E-mail", false, None, &[])),
            ]);
            page.render(&scanner, &profiles);

            let rows = page.rows.borrow();
            assert_eq!(rows.len(), 4);
            assert!(visible(&page.group));
            assert!(!visible(&page.empty));

            // §9: no rename action where the firmware refuses one — and no disabled
            // entry standing in for it either.
            for row in rows.iter() {
                assert!(visible(&row.label_text), "row {} lost its label", row.index);
                assert!(
                    !visible(&row.label_entry),
                    "row {} offers a rename the device would refuse",
                    row.index
                );
            }
            assert_eq!(rows[0].label_text.label(), "Scan to File");

            // §5: the divergence is a caption on the key that has one, and silence
            // everywhere else.
            assert!(visible(&rows[2].caption));
            assert_eq!(
                rows[2].caption.label(),
                "This key is labelled \"Scan to OCR\" but runs the document profile"
            );
            for index in [0, 1, 3] {
                assert!(!visible(&rows[index].caption), "row {index} nags");
            }

            // The profile column: the assignment, or the way to make one.
            assert_eq!(rows[0].profile_button.label().as_deref(), Some("Document"));
            assert_eq!(rows[1].profile_button.label().as_deref(), Some("Image"));
            assert_eq!(
                rows[3].profile_button.label().as_deref(),
                Some("Assign profile")
            );

            // Only what the key overrides: key 1 changes `format`, key 0 changes nothing.
            assert_eq!(rows[1].summary.label(), "Format: png");
            assert!(visible(&rows[1].summary));
            assert!(!visible(&rows[0].summary));

            // Unassigned keys have no profile options to open.
            assert!(!rows[3].configure.is_sensitive());
            assert!(rows[0].configure.is_sensitive());

            // The default profile group writes `Scanner1.DefaultProfile` and shows what
            // that profile is set to.
            assert_eq!(page.default_button.label().as_deref(), Some("Document"));
            assert_eq!(page.default_summary.label(), "Output folder: /srv/scans");

            // Picking a profile is this action, resolved from the row the way a menu
            // item resolves it — so this is the picker's own wiring, not a shortcut past
            // it. What it produces is a property write and *nothing else*: the button
            // still reads "Assign profile" until the daemon says otherwise.
            rows[3]
                .root
                .activate_action(
                    &format!("button.{PROFILE_ACTION}"),
                    Some(&"image".to_variant()),
                )
                .expect("the row must carry the action its menu names");
            assert_eq!(
                rows[3].profile_button.label().as_deref(),
                Some("Assign profile"),
                "the row moved before the daemon confirmed"
            );
            match sent.try_recv() {
                Ok(BusCommand::SetButtonProfile { path, kind }) => {
                    assert!(path.ends_with("/button/3"), "wrote to {path}");
                    assert_eq!(kind, Some(ProfileKind::Image));
                }
                other => panic!("expected a Button1.Profile write, got {other:?}"),
            }

            // Re-picking what is already assigned writes nothing.
            rows[0]
                .root
                .activate_action(
                    &format!("button.{PROFILE_ACTION}"),
                    Some(&"document".to_variant()),
                )
                .unwrap();
            assert!(sent.is_empty(), "a no-op pick still rewrote the config");

            page.root
                .activate_action(
                    &format!("scanner.{DEFAULT_PROFILE_ACTION}"),
                    Some(&"image".to_variant()),
                )
                .expect("the page must carry the default-profile action");
            match sent.try_recv() {
                Ok(BusCommand::SetDefaultProfile { path, kind }) => {
                    assert_eq!(path, scanbus_core::path::scanner(&scanner_id()));
                    assert_eq!(kind, Some(ProfileKind::Image));
                }
                other => panic!("expected a Scanner1.DefaultProfile write, got {other:?}"),
            }
            drop(rows);

            // --- The same page, for a device that does allow renaming. ---
            let mut hp = brother(1, true);
            hp.buttons = BTreeMap::from([(
                0,
                button(0, "Shortcut 1", true, Some(ProfileKind::Image), &[]),
            )]);
            page.render(&hp, &profiles);

            let rows = page.rows.borrow();
            assert_eq!(rows.len(), 1);
            assert!(visible(&rows[0].label_entry));
            assert!(!visible(&rows[0].label_text));
            assert_eq!(rows[0].label_entry.text(), "Shortcut 1");
            assert!(!visible(&rows[0].caption), "no opinion is not a divergence");
            drop(rows);

            // --- A scanner with no keys at all: an explanation, not an empty group. ---
            page.render(&brother(0, false), &profiles);
            assert!(page.rows.borrow().is_empty());
            assert!(!visible(&page.group));
            assert!(visible(&page.empty));
            assert_eq!(page.empty.label(), empty_state_text(0));
        }
    }
}
