mod bus;
mod error;
mod scanners;
mod store;
mod window;

use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::bus::BusHandle;
use crate::scanners::ScannerListModel;

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("org.scanbus.Gui")
        .build();

    let bus = Rc::new(BusHandle::start(scanbus_client::Bus::Session));
    let scanners = Rc::new(ScannerListModel::new());

    {
        let scanners = Rc::clone(&scanners);
        let events = bus.events();
        app.connect_startup(move |_| {
            let scanners = Rc::clone(&scanners);
            let events = events.clone();

            glib::spawn_future_local(async move {
                while let Ok(event) = events.recv().await {
                    if let Err(error) = scanners.apply_event(event) {
                        eprintln!("scanbus-gui: dropping store event: {error}");
                    }
                }
            });
        });
    }

    {
        let bus = Rc::clone(&bus);
        let scanners = Rc::clone(&scanners);
        app.connect_activate(move |app| {
            let window = window::build_window(app, Rc::clone(&scanners), bus.commands());
            window.present();
        });
    }

    app.run()
}
