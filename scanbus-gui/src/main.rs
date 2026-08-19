mod autostart;
mod bus;
mod buttons;
mod details;
mod error;
#[cfg(all(test, feature = "gtk-tests"))]
mod gtk_tests;
mod lifecycle;
mod notify;
mod options;
mod profiles;
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

/// The widget definitions build.rs compiled out of `data/ui/*.blp`.
///
/// Called before anything builds a widget, so a template lookup can never race it.
/// A function rather than a line in `main` because the resource test below needs the
/// same registration and must not duplicate the bundle name.
fn register_resources() {
    gio::resources_register_include!("scanbus.gresource")
        .expect("register the gresource build.rs bundled into the binary");
}

fn main() -> glib::ExitCode {
    register_resources();

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

/// The `.blp` → `.ui` → gresource path of §2.1, checked end to end.
///
/// The bundle is generated by `build.rs` from whatever `data/ui/` holds, so nothing in
/// the crate lists the templates; this walks the directory instead of a hand-written
/// table, and a `.blp` added without a rebuild — or renamed without its resource
/// following — fails here rather than at the first `present()`.
///
/// No display and no `libadwaita::init` needed: GResource is GIO, so this stays outside
/// the single-threaded suite of `gtk_tests.rs`, which is behind `--features gtk-tests`.
#[cfg(test)]
mod resources {
    use std::collections::HashMap;
    use std::fs;
    use std::path::Path;

    use gtk4::gio;

    use super::register_resources;

    /// `build.rs`'s own `RESOURCE_PREFIX`, which is where the bundle puts every `.ui`.
    /// It lives here rather than beside [`register_resources`] because nothing outside
    /// this test looks a template up yet; the first port issue lifts it out.
    const UI_RESOURCE_PREFIX: &str = "/org/scanbus/Gui/ui";

    /// The class a `.blp` declares, out of its `template $Class : Parent` line.
    fn template_class(source: &str) -> Option<&str> {
        source
            .lines()
            .find_map(|line| line.trim().strip_prefix("template $"))?
            .split([' ', ':', '{'])
            .next()
            .filter(|class| !class.is_empty())
    }

    #[test]
    fn every_blueprint_is_bundled_under_its_own_template_class() {
        register_resources();

        let ui_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/ui");
        let mut blueprints: Vec<_> = fs::read_dir(&ui_dir)
            .unwrap_or_else(|e| panic!("read {}: {e}", ui_dir.display()))
            .map(|entry| entry.expect("read dir entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "blp"))
            .collect();
        blueprints.sort();

        assert!(
            !blueprints.is_empty(),
            "no .blp under {} — build.rs would have produced an empty bundle",
            ui_dir.display()
        );

        // Class name → the file that declared it. A template instantiated by name cannot
        // tell two classes apart, so a duplicate is a silent wrong-widget bug.
        let mut classes: HashMap<String, String> = HashMap::new();

        for blueprint in &blueprints {
            let stem = blueprint
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_else(|| panic!("non-UTF-8 blueprint name: {}", blueprint.display()));
            assert_ne!(
                stem, "scanbus",
                "data/ui/scanbus.blp is the one-file transcription this split replaced"
            );

            let source = fs::read_to_string(blueprint)
                .unwrap_or_else(|e| panic!("read {}: {e}", blueprint.display()));
            let class = template_class(&source).unwrap_or_else(|| {
                panic!(
                    "{stem}.blp declares no `template $Class`: every widget here is \
                     instantiated as a gtk::CompositeTemplate subclass, not looked up \
                     out of a gtk::Builder"
                )
            });

            if let Some(first) = classes.insert(class.to_owned(), stem.to_owned()) {
                panic!("both {first}.blp and {stem}.blp declare `template ${class}`");
            }

            let resource = format!("{UI_RESOURCE_PREFIX}/{stem}.ui");
            let data = gio::resources_lookup_data(&resource, gio::ResourceLookupFlags::NONE)
                .unwrap_or_else(|e| panic!("{resource} is not in the bundle: {e}"));
            let ui = String::from_utf8(data.to_vec())
                .unwrap_or_else(|e| panic!("{resource} is not UTF-8 XML: {e}"));

            assert!(
                ui.contains(&format!("<template class=\"{class}\"")),
                "{resource} does not carry `template {class}`, so {stem}.blp and the \
                 bundled .ui have come apart"
            );
        }
    }
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
