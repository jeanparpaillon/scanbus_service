use std::cell::{Cell, RefCell};

use gtk::gio;
use gtk::glib;
use gtk::prelude::ApplicationExtManual;
use gtk4 as gtk;
use libadwaita as adw;

#[derive(Default)]
pub struct AppLifecycle {
    background_held: Cell<bool>,
    hold_guard: RefCell<Option<gio::ApplicationHoldGuard>>,
    window: RefCell<glib::WeakRef<adw::ApplicationWindow>>,
}

impl AppLifecycle {
    pub fn hold_background(&self, app: &adw::Application) {
        if !self.background_held.replace(true) {
            *self.hold_guard.borrow_mut() = Some(app.hold());
        }
    }

    pub fn is_background_held(&self) -> bool {
        self.background_held.get()
    }

    pub fn current_window(&self) -> Option<adw::ApplicationWindow> {
        self.window.borrow().upgrade()
    }

    pub fn track_window(&self, window: &adw::ApplicationWindow) {
        let weak = {
            let weak = glib::WeakRef::new();
            weak.set(Some(window));
            weak
        };
        *self.window.borrow_mut() = weak;
    }
}
