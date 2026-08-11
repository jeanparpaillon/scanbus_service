use std::rc::Rc;

use async_channel::Sender;
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::bus::BusCommand;
use crate::scanners::{ScannerListModel, ScannersPane};
use crate::store::DiscoveryState;

pub fn build_window(
    app: &adw::Application,
    scanners: Rc<ScannerListModel>,
    commands: Sender<BusCommand>,
) -> adw::ApplicationWindow {
    let find_button = gtk::Button::with_label("Find scanners…");
    let spinner = gtk::Spinner::new();
    spinner.set_spinning(false);
    spinner.set_visible(false);
    let pane_commands = commands.clone();

    let header = adw::HeaderBar::new();
    header.pack_end(&spinner);
    header.pack_end(&find_button);

    let sections = gtk::ListBox::new();
    sections.add_css_class("navigation-sidebar");
    sections.append(&row("Scanners"));
    sections.append(&row("Profiles"));

    let footer = gtk::ListBox::new();
    footer.add_css_class("navigation-sidebar");
    footer.append(&row("Settings"));

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

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    content.append(&sidebar);
    content.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    content.append(scanners_pane.widget());

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&header);
    root.append(&content);

    let overlay = adw::ToastOverlay::new();
    overlay.set_child(Some(&root));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Scanbus")
        .default_width(1180)
        .default_height(720)
        .content(&overlay)
        .build();

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
        scanners.connect_toast(move |message| {
            overlay.add_toast(adw::Toast::new(&message));
        });
    }

    {
        let scanners = Rc::clone(&scanners);
        let commands = commands.clone();
        window.connect_hide(move |_| {
            if scanners.begin_discovery_stop() {
                let _ = commands.try_send(BusCommand::StopDiscovery { quiet: true });
            }
        });
    }

    window
}

fn row(title: &str) -> gtk::ListBoxRow {
    let label = gtk::Label::new(Some(title));
    label.set_xalign(0.0);
    label.set_margin_top(12);
    label.set_margin_bottom(12);
    label.set_margin_start(12);
    label.set_margin_end(12);

    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&label));
    row
}
