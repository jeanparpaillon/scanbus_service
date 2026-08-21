//! The crate's only widget test, and the only place `libadwaita::init` is called.
//!
//! GTK pins itself to the thread that initialised it, and cargo runs every `#[test]` on a
//! thread of its own. A second test function would therefore drive widgets from a thread
//! GTK has not been initialised on — which mostly works, until it does not, in CI. So
//! there is one test, each module contributes a `widget_checks::run` to it, and the
//! assertions still live next to the code they are about.
//!
//! The suite is behind `--features gtk-tests` and skipped rather than failed with no
//! display: the same deal the daemon's bus tests make with `dbus-daemon`. A headless
//! runner should not turn the suite red over a dependency that is not a build dependency.
//! `xvfb-run cargo test -p scanbus-gui --features gtk-tests` is what runs it for real.

#[test]
fn the_widget_layer_binds_to_the_store() {
    if libadwaita::init().is_err() {
        eprintln!("skipping: no display for GTK");
        return;
    }

    crate::buttons::widget_checks::run();
    crate::details::widget_checks::run();
    crate::options::widget_checks::run();
    crate::profiles::widget_checks::run();
    // Also runs `scanner_row.rs`'s checks, on rows its own factories built.
    crate::scanners::widget_checks::run();
    crate::settings::widget_checks::run();
    crate::unpair_dialog::widget_checks::run();
    // Last, because it is the only one that needs a registered `adw::Application`
    // and it instantiates every pane the stack holds, this page included.
    crate::window::widget_checks::run();
}
