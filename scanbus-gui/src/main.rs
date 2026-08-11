mod bus;
mod error;
mod store;

use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::bus::BusHandle;
use crate::store::Store;

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("org.scanbus.Gui")
        .build();

    let bus = Rc::new(BusHandle::start(scanbus_client::Bus::Session));
    let store = Rc::new(RefCell::new(Store::default()));

    {
        let store = Rc::clone(&store);
        let events = bus.events();
        app.connect_startup(move |_| {
            let store = Rc::clone(&store);
            let events = events.clone();

            glib::spawn_future_local(async move {
                while let Ok(event) = events.recv().await {
                    if let Err(error) = store.borrow_mut().apply(event) {
                        eprintln!("scanbus-gui: dropping store event: {error}");
                    }
                }
            });
        });
    }

    {
        let bus = Rc::clone(&bus);
        app.connect_activate(move |app| {
            let _commands = bus.commands();
            let window = build_window(app);
            window.present();
        });
    }

    app.run()
}

fn build_window(app: &adw::Application) -> adw::ApplicationWindow {
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

    let placeholder = gtk::Box::new(gtk::Orientation::Vertical, 12);
    placeholder.set_hexpand(true);
    placeholder.set_vexpand(true);
    placeholder.set_margin_top(24);
    placeholder.set_margin_bottom(24);
    placeholder.set_margin_start(24);
    placeholder.set_margin_end(24);

    let title = gtk::Label::new(Some("Scanbus"));
    title.add_css_class("title-1");
    title.set_xalign(0.0);
    placeholder.append(&title);

    let detail = gtk::Label::new(Some(
        "The scanner, profile and settings views land in this pane.",
    ));
    detail.set_xalign(0.0);
    detail.set_wrap(true);
    placeholder.append(&detail);

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    content.append(&sidebar);
    content.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    content.append(&placeholder);

    adw::ApplicationWindow::builder()
        .application(app)
        .title("Scanbus")
        .default_width(980)
        .default_height(640)
        .content(&content)
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
