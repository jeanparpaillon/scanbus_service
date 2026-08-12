mod bus;
mod buttons;
mod error;
mod scanners;
mod store;
mod window;

use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::bus::{BusCommand, BusEvent, BusHandle};
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
                    match event {
                        BusEvent::Store(event) => {
                            if let Err(error) = scanners.apply_event(event) {
                                eprintln!("scanbus-gui: dropping store event: {error}");
                            }
                        }
                        BusEvent::DiscoveryActive(true) => scanners.mark_discovery_active(),
                        BusEvent::DiscoveryActive(false) => scanners.mark_discovery_idle(),
                        BusEvent::Toast(toast) => {
                            // A toast means the daemon refused something a widget had
                            // already shown as done. The store never moved, so
                            // re-rendering from it is the revert.
                            scanners.emit_toast_spec(toast);
                            scanners.refresh();
                        }
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

    install_signal_stop(&app, &bus.commands(), libc::SIGINT);
    install_signal_stop(&app, &bus.commands(), libc::SIGTERM);

    {
        let commands = bus.commands();
        let scanners = Rc::clone(&scanners);
        app.connect_shutdown(move |_| {
            if scanners.begin_discovery_stop() {
                let _ = commands.try_send(BusCommand::StopDiscovery { quiet: true });
            }
        });
    }

    app.run()
}

fn install_signal_stop(
    app: &adw::Application,
    commands: &async_channel::Sender<BusCommand>,
    signal: i32,
) {
    let app = app.downgrade();
    let commands = commands.clone();

    glib::source::unix_signal_add_local(signal, move || {
        let _ = commands.try_send(BusCommand::StopDiscovery { quiet: true });
        if let Some(app) = app.upgrade() {
            app.quit();
        }
        glib::ControlFlow::Break
    });
}
