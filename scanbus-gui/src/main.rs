mod autostart;
mod bus;
mod buttons;
mod error;
mod lifecycle;
mod notify;
mod scanners;
mod store;
mod window;

use std::rc::Rc;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::bus::{BusCommand, BusEvent, BusHandle};
use crate::lifecycle::AppLifecycle;
use crate::notify::Notifier;
use crate::scanners::ScannerListModel;

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("org.scanbus.Gui")
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    let bus = Rc::new(BusHandle::start(scanbus_client::Bus::Session));
    let scanners = Rc::new(ScannerListModel::new());
    let lifecycle = Rc::new(AppLifecycle::default());
    let notifier = Rc::new(Notifier::new(
        &app,
        Rc::clone(&scanners),
        Rc::clone(&lifecycle),
    ));

    {
        let scanners = Rc::clone(&scanners);
        let notifier = Rc::clone(&notifier);
        let events = bus.events();
        let app = app.clone();
        app.connect_startup(move |_| {
            let scanners = Rc::clone(&scanners);
            let notifier = Rc::clone(&notifier);
            let events = events.clone();

            glib::spawn_future_local(async move {
                while let Ok(event) = events.recv().await {
                    match event {
                        BusEvent::Store(event) => {
                            notifier.handle_store_event(&event);
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
        let app = app.clone();
        let quit = gio::SimpleAction::new("quit", None);
        let quit_app = app.clone();
        quit.connect_activate(move |_, _| quit_app.quit());
        app.add_action(&quit);
    }

    {
        let lifecycle = Rc::clone(&lifecycle);
        app.connect_command_line(move |app, command_line| {
            if launch_mode(command_line.arguments()) == LaunchMode::Background {
                lifecycle.hold_background(app);
                return 0.into();
            }

            app.activate();
            0.into()
        });
    }

    {
        let bus = Rc::clone(&bus);
        let scanners = Rc::clone(&scanners);
        let lifecycle = Rc::clone(&lifecycle);
        app.connect_activate(move |app| {
            if let Some(window) = lifecycle.current_window() {
                window.present();
                return;
            }

            let window = window::build_window(
                app,
                Rc::clone(&scanners),
                bus.commands(),
                Rc::clone(&lifecycle),
            );
            lifecycle.track_window(&window);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchMode {
    Activate,
    Background,
}

fn launch_mode(arguments: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>) -> LaunchMode {
    if arguments
        .into_iter()
        .skip(1)
        .any(|argument| argument.as_ref() == "--background")
    {
        LaunchMode::Background
    } else {
        LaunchMode::Activate
    }
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

#[cfg(test)]
mod tests {
    use super::{LaunchMode, launch_mode};

    #[test]
    fn background_flag_selects_background_mode() {
        let mode = launch_mode(["scanbus-gui", "--background"]);
        assert_eq!(mode, LaunchMode::Background);
    }

    #[test]
    fn plain_launch_activates_the_window() {
        let mode = launch_mode(["scanbus-gui"]);
        assert_eq!(mode, LaunchMode::Activate);
    }
}
