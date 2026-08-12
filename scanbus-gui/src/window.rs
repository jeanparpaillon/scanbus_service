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
use crate::profiles::ProfilesPage;
use crate::scanners::{ScannerListModel, ScannersPane, ToastAction};
use crate::store::{DiscoveryState, ServiceState};

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
    let profiles_page = Rc::new(ProfilesPage::new(commands.clone()));

    let page_stack = gtk::Stack::new();
    page_stack.set_hexpand(true);
    page_stack.set_vexpand(true);
    page_stack.add_named(scanners_pane.widget(), Some("scanners"));
    page_stack.add_named(profiles_page.widget(), Some("profiles"));

    {
        let model = Rc::clone(&scanners);
        let profiles_page = Rc::clone(&profiles_page);
        let render = move || {
            profiles_page.render(&model.profiles(), &model.service_details().profile_types);
        };
        render();
        scanners.connect_changed(render);
    }

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    content.append(&sidebar);
    content.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    content.append(&page_stack);

    let overlay = adw::ToastOverlay::new();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&header);
    root.append(&content);
    overlay.set_child(Some(&root));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Scanbus")
        .default_width(1180)
        .default_height(720)
        .content(&overlay)
        .build();

    page_stack.add_named(
        &settings_page(
            &window,
            &page_stack,
            Rc::clone(&scanners),
            commands.clone(),
            &overlay,
        ),
        Some("settings"),
    );

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

fn settings_page(
    window: &adw::ApplicationWindow,
    page_stack: &gtk::Stack,
    model: Rc<ScannerListModel>,
    commands: Sender<BusCommand>,
    overlay: &adw::ToastOverlay,
) -> gtk::ScrolledWindow {
    let page = adw::PreferencesPage::new();
    let daemon_group = adw::PreferencesGroup::builder()
        .title("Daemon")
        .description("Read the scanbus daemon state without starting it, then start it explicitly when it is only activatable.")
        .build();

    let daemon_row = adw::ActionRow::builder().title("Service").build();
    let start_button = gtk::Button::with_label("Start");
    start_button.set_valign(gtk::Align::Center);
    daemon_row.add_suffix(&start_button);
    daemon_group.add(&daemon_row);

    let version_row = adw::ActionRow::builder().title("Version").build();
    daemon_group.add(&version_row);

    let backends_row = adw::ActionRow::builder().title("Backends").build();
    daemon_group.add(&backends_row);

    let profiles_row = adw::ActionRow::builder().title("Profile types").build();
    daemon_group.add(&profiles_row);
    page.add(&daemon_group);

    let output_group = adw::PreferencesGroup::builder()
        .title("Default output folders")
        .description("These folders are read-only here. Use Profiles to change them.")
        .build();
    let image_row = profile_folder_row("Image");
    let document_row = profile_folder_row("Document");
    output_group.add(&image_row);
    output_group.add(&document_row);
    page.add(&output_group);

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

    let about_group = adw::PreferencesGroup::builder().title("About").build();
    let about_row = adw::ActionRow::builder()
        .title("About Scanbus")
        .subtitle("GUI version, daemon version, repository and licence")
        .activatable(true)
        .build();
    about_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    about_group.add(&about_row);
    page.add(&about_group);

    {
        let commands = commands.clone();
        start_button.connect_clicked(move |_| {
            let _ = commands.try_send(BusCommand::StartService);
        });
    }

    {
        let page_stack = page_stack.clone();
        image_row.connect_activated(move |_| {
            page_stack.set_visible_child_name("profiles");
        });
    }

    {
        let page_stack = page_stack.clone();
        document_row.connect_activated(move |_| {
            page_stack.set_visible_child_name("profiles");
        });
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

    {
        let window = window.clone();
        let model = Rc::clone(&model);
        about_row.connect_activated(move |_| {
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
        });
    }

    {
        let model = Rc::clone(&model);
        let refresh_model = Rc::clone(&model);
        let daemon_row = daemon_row.clone();
        let start_button = start_button.clone();
        let version_row = version_row.clone();
        let backends_row = backends_row.clone();
        let profiles_row = profiles_row.clone();
        let image_row = image_row.clone();
        let document_row = document_row.clone();
        let refresh = move || {
            let details = refresh_model.service_details();
            match refresh_model.service_state() {
                ServiceState::Running => {
                    daemon_row.set_subtitle("Running");
                    start_button.set_visible(false);
                }
                ServiceState::Activatable => {
                    daemon_row.set_subtitle("Installed, but not running");
                    start_button.set_visible(true);
                    start_button.set_sensitive(true);
                }
                ServiceState::Absent => {
                    daemon_row.set_subtitle("Absent from this session bus");
                    start_button.set_visible(false);
                }
                ServiceState::Unknown => {
                    daemon_row.set_subtitle("Checking the bus…");
                    start_button.set_visible(false);
                }
            }

            version_row.set_subtitle(details.version.as_deref().unwrap_or("Unknown"));
            backends_row.set_subtitle(&joined(&details.backends));
            profiles_row.set_subtitle(&joined(&details.profile_types));
            set_folder_row(
                &image_row,
                details
                    .output_folders
                    .get(&scanbus_core::ProfileKind::Image),
            );
            set_folder_row(
                &document_row,
                details
                    .output_folders
                    .get(&scanbus_core::ProfileKind::Document),
            );
        };
        refresh();
        model.connect_changed(refresh);
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

fn joined(values: &[String]) -> String {
    if values.is_empty() {
        "-".to_owned()
    } else {
        values.join(", ")
    }
}

fn profile_folder_row(title: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .activatable(true)
        .build();
    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    row
}

fn set_folder_row(row: &adw::ActionRow, folder: Option<&String>) {
    row.set_subtitle(folder.map(String::as_str).unwrap_or("Unknown"));
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
