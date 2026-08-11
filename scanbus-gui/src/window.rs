use std::rc::Rc;

use async_channel::Sender;
use gtk::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::bus::BusCommand;
use crate::scanners::{ScannerListModel, ScannersPane};

pub fn build_window(
    app: &adw::Application,
    scanners: Rc<ScannerListModel>,
    commands: Sender<BusCommand>,
) -> adw::ApplicationWindow {
    let find_button = gtk::Button::with_label("Find scanners…");
    find_button.set_sensitive(false);

    let header = adw::HeaderBar::new();
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

    let scanners_pane = ScannersPane::new(Rc::clone(&scanners), commands);

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    content.append(&sidebar);
    content.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    content.append(scanners_pane.widget());

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&header);
    root.append(&content);

    adw::ApplicationWindow::builder()
        .application(app)
        .title("Scanbus")
        .default_width(1180)
        .default_height(720)
        .content(&root)
        .build()
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
