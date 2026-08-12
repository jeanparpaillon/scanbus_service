use std::rc::Rc;

use async_channel::Sender;
use gtk::gio;
use gtk::glib;
use gtk::glib::variant::ToVariant;
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::autostart;
use crate::bus::BusCommand;
use crate::lifecycle::AppLifecycle;
use crate::scanners::{ScannerListModel, ScannersPane, ToastAction};
use crate::store::DiscoveryState;

pub fn build_window(
    app: &adw::Application,
    scanners: Rc<ScannerListModel>,
    commands: Sender<BusCommand>,
    lifecycle: Rc<AppLifecycle>,
) -> adw::ApplicationWindow {
    let find_button = gtk::Button::with_label("Find scanners…");
    let spinner = gtk::Spinner::new();
    spinner.set_spinning(false);
    spinner.set_visible(false);
    let pane_commands = commands.clone();
    let app_menu = gio::Menu::new();
    app_menu.append(Some("Quit"), Some("app.quit"));
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&app_menu)
        .build();

    let header = adw::HeaderBar::new();
    header.pack_end(&menu_button);
    header.pack_end(&spinner);
    header.pack_end(&find_button);

    let sections = gtk::ListBox::new();
    sections.add_css_class("navigation-sidebar");
    sections.set_selection_mode(gtk::SelectionMode::Single);
    let scanners_row = row("Scanners", "scanners");
    let profiles_row = row("Profiles", "profiles");
    sections.append(&scanners_row);
    sections.append(&profiles_row);

    let footer = gtk::ListBox::new();
    footer.add_css_class("navigation-sidebar");
    footer.set_selection_mode(gtk::SelectionMode::Single);
    let settings_row = row("Settings", "settings");
    footer.append(&settings_row);

    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    spacer.set_vexpand(true);
    sidebar.set_margin_top(18);
    sidebar.set_margin_bottom(18);
    sidebar.set_margin_start(18);
    sidebar.set_margin_end(18);
    sidebar.set_size_request(220, -1);
    sidebar.append(&sections);
    sidebar.append(&spacer);
    sidebar.append(&footer);

    let scanners_pane = ScannersPane::new(Rc::clone(&scanners), pane_commands);
    let profiles_placeholder = adw::StatusPage::builder()
        .title("Profiles are not editable yet")
        .description("Issue 10.7 owns the profile editor and option widgets.")
        .build();

    let page_stack = gtk::Stack::new();
    page_stack.set_hexpand(true);
    page_stack.set_vexpand(true);
    page_stack.add_named(scanners_pane.widget(), Some("scanners"));
    page_stack.add_named(&profiles_placeholder, Some("profiles"));

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    content.append(&sidebar);
    content.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    content.append(&page_stack);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&header);
    root.append(&content);

    let overlay = adw::ToastOverlay::new();
    overlay.set_child(Some(&root));
    page_stack.add_named(&settings_page(&overlay), Some("settings"));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Scanbus")
        .default_width(1180)
        .default_height(720)
        .content(&overlay)
        .build();

    {
        let footer = footer.clone();
        let page_stack = page_stack.clone();
        sections.connect_row_selected(move |_, row| {
            let Some(row) = row else {
                return;
            };
            footer.unselect_all();
            page_stack.set_visible_child_name(row.widget_name().as_str());
        });
    }

    {
        let sections = sections.clone();
        let page_stack = page_stack.clone();
        footer.connect_row_selected(move |_, row| {
            let Some(row) = row else {
                return;
            };
            sections.unselect_all();
            page_stack.set_visible_child_name(row.widget_name().as_str());
        });
    }

    sections.select_row(Some(&scanners_row));

    {
        let commands = commands.clone();
        let retry_pair = gio::SimpleAction::new("retry-pair", Some(&String::static_variant_type()));
        retry_pair.connect_activate(move |_, parameter| {
            let Some(parameter) = parameter else {
                return;
            };
            let Some(path) = parameter.get::<String>() else {
                return;
            };
            let _ = commands.try_send(BusCommand::Pair { path });
        });
        window.add_action(&retry_pair);
    }

    {
        let scanners = Rc::clone(&scanners);
        let commands = commands.clone();
        find_button.connect_clicked(move |_| match scanners.discovery_state() {
            DiscoveryState::Idle => {
                if scanners.begin_discovery_start() {
                    let _ = commands.try_send(BusCommand::StartDiscovery);
                }
            }
            DiscoveryState::Starting | DiscoveryState::Active => {
                if scanners.begin_discovery_stop() {
                    let _ = commands.try_send(BusCommand::StopDiscovery { quiet: false });
                }
            }
            DiscoveryState::Stopping => {}
        });
    }

    {
        let scanners = Rc::clone(&scanners);
        let find_button = find_button.clone();
        let spinner = spinner.clone();
        let model = Rc::clone(&scanners);
        scanners.connect_changed(move || {
            let state = model.discovery_state();
            let busy = !matches!(state, DiscoveryState::Idle);
            spinner.set_visible(busy);
            spinner.set_spinning(busy);

            match state {
                DiscoveryState::Idle => {
                    find_button.set_label("Find scanners…");
                    find_button.set_sensitive(true);
                }
                DiscoveryState::Starting | DiscoveryState::Active => {
                    find_button.set_label("Stop");
                    find_button.set_sensitive(true);
                }
                DiscoveryState::Stopping => {
                    find_button.set_label("Stop");
                    find_button.set_sensitive(false);
                }
            }
        });
    }

    {
        let overlay = overlay.clone();
        scanners.connect_toast(move |toast| {
            let toast_widget = adw::Toast::new(&toast.message);
            if let Some(label) = &toast.button_label {
                toast_widget.set_button_label(Some(label));
            }
            if let Some(action) = &toast.action {
                match action {
                    ToastAction::RetryPair { path } => {
                        toast_widget.set_action_name(Some("win.retry-pair"));
                        toast_widget.set_action_target_value(Some(&path.to_variant()));
                    }
                }
            }
            overlay.add_toast(toast_widget);
        });
    }

    {
        let scanners = Rc::clone(&scanners);
        let commands = commands.clone();
        let lifecycle = Rc::clone(&lifecycle);
        window.connect_close_request(move |window| {
            if scanners.begin_discovery_stop() {
                let _ = commands.try_send(BusCommand::StopDiscovery { quiet: true });
            }
            if lifecycle.is_background_held() {
                window.destroy();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
    }

    window
}

fn settings_page(overlay: &adw::ToastOverlay) -> gtk::ScrolledWindow {
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title("Background mode")
        .description("The packaged GNOME session starts scanbus-gui in the background at login through XDG autostart.")
        .build();

    let row = adw::ActionRow::builder()
        .title("Start in the background at login")
        .build();
    let toggle = gtk::Switch::new();
    toggle.set_valign(gtk::Align::Center);
    row.set_activatable_widget(Some(&toggle));
    row.add_suffix(&toggle);
    group.add(&row);
    page.add(&group);

    match autostart::is_enabled() {
        Ok(enabled) => {
            toggle.set_active(enabled);
            row.set_subtitle(&autostart_subtitle(enabled));
        }
        Err(error) => {
            toggle.set_sensitive(false);
            row.set_subtitle(&format!("Could not read the autostart override: {error}"));
        }
    }

    {
        let row = row.clone();
        let overlay = overlay.clone();
        let syncing = Rc::new(std::cell::Cell::new(false));
        let syncing_for_cb = Rc::clone(&syncing);
        toggle.connect_active_notify(move |toggle| {
            if syncing_for_cb.get() {
                return;
            }

            let enabled = toggle.is_active();
            if let Err(error) = autostart::set_enabled(enabled) {
                syncing_for_cb.set(true);
                toggle.set_active(!enabled);
                syncing_for_cb.set(false);
                overlay.add_toast(adw::Toast::new(&format!(
                    "Could not update the autostart override: {error}"
                )));
                return;
            }

            row.set_subtitle(&autostart_subtitle(enabled));
        });
    }

    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_child(Some(&page));
    scroller
}

fn autostart_subtitle(enabled: bool) -> String {
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

fn row(title: &str, page: &str) -> gtk::ListBoxRow {
    let label = gtk::Label::new(Some(title));
    label.set_xalign(0.0);
    label.set_margin_top(12);
    label.set_margin_bottom(12);
    label.set_margin_start(12);
    label.set_margin_end(12);

    let row = gtk::ListBoxRow::new();
    row.set_widget_name(page);
    row.set_child(Some(&label));
    row
}
