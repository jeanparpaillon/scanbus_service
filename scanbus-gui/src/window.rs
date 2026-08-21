use std::cell::OnceCell;
use std::rc::Rc;

use async_channel::Sender;
use gtk::glib::variant::ToVariant;
use gtk::{CompositeTemplate, TemplateChild, gio, glib};
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
// Brings in gtk4's subclass prelude as well, so `WidgetImpl` and friends arrive with
// `AdwApplicationWindowImpl` rather than from a second import.
use libadwaita::subclass::prelude::*;

use crate::bus::BusCommand;
use crate::lifecycle::AppLifecycle;
use crate::profiles::ProfilesPage;
use crate::scanners::{ScannerListModel, ScannersPane, ToastAction};
use crate::settings::SettingsPage;
use crate::store::DiscoveryState;

/// The private instance struct GObject owns; [`ScanbusWindow`] is the handle everything
/// else holds.
mod imp {
    use super::*;

    // No `Debug`: two of the three handles below are plain Rust types that do not
    // derive it, and a window's interesting state is the store's, not this struct's.
    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/org/scanbus/Gui/ui/window.ui")]
    pub struct ScanbusWindow {
        /// `ScannerListModel::connect_toast` adds to this, and the settings page
        /// reaches it to report a failed autostart write.
        #[template_child]
        pub toast_overlay: TemplateChild<adw::ToastOverlay>,
        #[template_child]
        pub spinner: TemplateChild<gtk::Spinner>,
        #[template_child]
        pub find_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub sections: TemplateChild<gtk::ListBox>,
        /// Selected at the end of construction, which is what makes Scanners the page
        /// the window opens on.
        #[template_child]
        pub scanners_row: TemplateChild<gtk::ListBoxRow>,
        #[template_child]
        pub footer: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub page_stack: TemplateChild<gtk::Stack>,

        /// The three non-GObject handles the callbacks and the store registrations
        /// need.
        ///
        /// They are cells set after construction rather than construct properties
        /// because none of the three is a `glib::Value`: making them properties would
        /// mean a boxed GObject wrapper per handle, and a `#[template_callback]`
        /// reaches them through `self.imp()` either way.
        pub scanners: OnceCell<Rc<ScannerListModel>>,
        pub commands: OnceCell<Sender<BusCommand>>,
        pub lifecycle: OnceCell<Rc<AppLifecycle>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ScanbusWindow {
        /// Must match `template $ScanbusWindow` in `window.blp` exactly: this is the
        /// name the builder resolves the template against, so the two are one string
        /// spelled in two files.
        const NAME: &'static str = "ScanbusWindow";
        type Type = super::ScanbusWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            // `bind_template_instance_callbacks`, not `bind_template_callbacks`: the
            // `#[gtk::template_callbacks]` block is on the wrapper type, so that the
            // handlers read the same `self.imp()` the rest of `window.rs` does.
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for ScanbusWindow {}
    impl WidgetImpl for ScanbusWindow {}
    impl WindowImpl for ScanbusWindow {}
    impl ApplicationWindowImpl for ScanbusWindow {}
    impl AdwApplicationWindowImpl for ScanbusWindow {}
}

glib::wrapper! {
    /// The composite-template subclass `window.blp` declares, and the only place the
    /// window's widget tree is named.
    ///
    /// Nothing in `imp` is constructed by Rust: `init_template` fills each
    /// `#[template_child]` from the id of the same name in the compiled `.ui`, so a
    /// field with no matching id — or an id whose type has drifted — fails at
    /// instantiation rather than leaving a widget that silently never appears. That is
    /// the drift check §10 of the design doc asks for.
    pub struct ScanbusWindow(ObjectSubclass<imp::ScanbusWindow>)
        @extends adw::ApplicationWindow, gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl ScanbusWindow {
    /// Builds the window and hands it the three non-GObject handles its callbacks need.
    ///
    /// The one entry point `main.rs` has, and the only place this window is built.
    /// The cells are filled here, once, before the window has been presented or
    /// returned, so a `#[template_callback]` may read them as already set — nothing can
    /// reach a callback before `new` gives the window back.
    pub fn new(
        app: &adw::Application,
        scanners: Rc<ScannerListModel>,
        commands: Sender<BusCommand>,
        lifecycle: Rc<AppLifecycle>,
    ) -> Self {
        let window: Self = glib::Object::builder().property("application", app).build();

        // Every cell is empty: the object was built two lines up and this is its only
        // constructor, so `set` cannot fail. The asserts say that rather than dropping a
        // handle on the floor should it ever stop being true — and `Rc<ScannerListModel>`
        // is not `Debug`, so `expect` is not available here anyway.
        let imp = window.imp();
        assert!(imp.scanners.set(scanners).is_ok(), "scanners set twice");
        assert!(imp.commands.set(commands).is_ok(), "commands set twice");
        assert!(imp.lifecycle.set(lifecycle).is_ok(), "lifecycle set twice");

        // `win.retry-pair` stays what it is today: a runtime `SimpleAction` with a string
        // target, added to the window rather than declared in `window.blp`. Blueprint can
        // name an action a widget activates, but it cannot install one — and nothing here
        // activates it from a widget anyway: the only emitter is the toast raised below,
        // which carries the scanner's object path as the action target.
        //
        // The closure captures a clone of the command channel and not the window. A
        // strong `self` would be a cycle — the window owns its action map, which owns the
        // action, which owns this closure — and the sender is all the handler needs.
        {
            let commands = imp.commands.get().expect("commands set just above").clone();
            let retry_pair =
                gio::SimpleAction::new("retry-pair", Some(&String::static_variant_type()));
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

        // The three pages of `page_stack`, added by instance because `window.blp`
        // declares the stack empty: a `Gtk.StackPage` there has to name a child *class*,
        // and a builder-instantiated child gets no constructor arguments — all three
        // pages are handed the store and the bus channel when they are built. `add_named`
        // takes an instance and so does not care what class it is, which is what lets
        // Profiles sit in the same stack while it is still a plain widget ([10.23]).
        let model = Rc::clone(imp.scanners.get().expect("scanners set just above"));
        let commands = imp.commands.get().expect("commands set just above").clone();

        let scanners_pane = ScannersPane::new(Rc::clone(&model), commands.clone());
        let profiles_page = Rc::new(ProfilesPage::new(commands.clone()));
        imp.page_stack.add_named(&scanners_pane, Some("scanners"));
        imp.page_stack
            .add_named(profiles_page.widget(), Some("profiles"));
        imp.page_stack.add_named(
            &SettingsPage::new(Rc::clone(&model), commands),
            Some("settings"),
        );

        // Selecting the row is what opens the window on Scanners: the template's own
        // `row-selected` handler switches the stack. It has to follow the `add_named`
        // calls above — `set_visible_child_name` for a child the stack does not have yet
        // does nothing, and the window would come up on an empty pane.
        imp.sections.select_row(Some(&*imp.scanners_row));

        // The Profiles page renders from the store like every other page, but it is not
        // a subclass yet, so its registration stays here instead of moving into its own
        // constructor with [10.23]. It captures the page, which has no other owner, and
        // the model — but not the window, which is the one that has to stay finalisable.
        {
            let model = Rc::clone(&model);
            let profiles_page = Rc::clone(&profiles_page);
            let render = move || {
                profiles_page.render(&model.profiles(), &model.service_details().profile_types);
            };
            render();
            imp.scanners
                .get()
                .expect("scanners set just above")
                .connect_changed(render);
        }

        // Registered once, here, rather than from a `#[template_callback]`: `render` is
        // driven by the store rather than by a signal on any widget. The closure holds a
        // weak reference because `ScannerListModel` keeps every callback it is given for
        // its own lifetime, and the model is held in `imp` above — a strong `self` would
        // be a cycle through the store, and the window would never be finalised.
        let weak = window.downgrade();
        imp.scanners
            .get()
            .expect("scanners set just above")
            .connect_changed(move || {
                if let Some(window) = weak.upgrade() {
                    window.render();
                }
            });

        // The toast wiring, also unchanged in substance: `ScannerListModel` raises a spec
        // and this turns it into an `adw::Toast` on the overlay. It cannot move into the
        // `.blp` either — the message, the button label and the action target are all
        // written per toast from the store.
        //
        // It goes through `add_toast` on a weak handle rather than capturing the overlay
        // widget, for the same reason the render registration above does: `connect_toast`
        // keeps every callback for the model's lifetime, so a captured overlay would
        // outlive the window that owns it.
        let weak_for_toast = window.downgrade();
        imp.scanners
            .get()
            .expect("scanners set just above")
            .connect_toast(move |toast| {
                let Some(window) = weak_for_toast.upgrade() else {
                    return;
                };
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
                window.add_toast(toast_widget);
            });

        // Nothing rendered the header bar before it was presented until now: the
        // hand-built button this replaces already carried the Idle label the template
        // declares. It matters when a window is built while discovery is already running
        // — a `--background` close and a later re-activation — where the store is ahead
        // of the template.
        window.render();

        window
    }

    /// Writes the header bar from the store: the spinner, and the find button's label
    /// and sensitivity.
    ///
    /// The whole of what this window renders itself — every page in the stack renders
    /// its own content. It is one button because it is one action in both directions:
    /// the label is what says which direction the next click takes.
    fn render(&self) {
        let imp = self.imp();
        let state = imp
            .scanners
            .get()
            .expect("scanners set in `new`")
            .discovery_state();

        let busy = !matches!(state, DiscoveryState::Idle);
        imp.spinner.set_visible(busy);
        imp.spinner.set_spinning(busy);

        match state {
            DiscoveryState::Idle => {
                imp.find_button.set_label("Find scanners…");
                imp.find_button.set_sensitive(true);
            }
            DiscoveryState::Starting | DiscoveryState::Active => {
                imp.find_button.set_label("Stop");
                imp.find_button.set_sensitive(true);
            }
            // A stop is in flight: the label still says what is being waited for, and
            // the button refuses further clicks until the daemon answers.
            DiscoveryState::Stopping => {
                imp.find_button.set_label("Stop");
                imp.find_button.set_sensitive(false);
            }
        }
    }

    /// Switches the page stack by name, leaving both sidebar selections alone.
    ///
    /// What the settings page's two output rows call to jump to Profiles. Not touching
    /// the selection is deliberate: the closure this replaces did not either, so the
    /// footer's Settings row stays selected over the Profiles page exactly as today.
    // Reached only from `settings.rs`, whose caller is a template callback.
    pub(crate) fn show_page(&self, name: &str) {
        self.imp().page_stack.set_visible_child_name(name);
    }

    /// Raises a toast on the window's overlay.
    ///
    /// The overlay is a `#[template_child]` of this window, so a page inside it — the
    /// settings page reporting a failed autostart write — asks the window rather than
    /// holding the overlay itself.
    pub(crate) fn add_toast(&self, toast: adw::Toast) {
        self.imp().toast_overlay.add_toast(toast);
    }
}

/// The four handlers `window.blp` names, in the order the template mentions them.
///
/// Each is declared `swapped` in the `.blp`, which is what puts the window in `&self`:
/// GtkBuilder hands a template handler the emitting widget first and the template
/// instance last, and `swapped` exchanges the two. So every emitter here — the button,
/// either list box — is the argument that gets dropped, and the state comes off
/// `self.imp()` instead of out of a captured `Rc`.
#[gtk::template_callbacks]
impl ScanbusWindow {
    /// Stops discovery, then destroys instead of closing while the background hold is
    /// taken.
    ///
    /// Returning `Propagation::Stop` after `destroy()` is what §4 rests on: the default
    /// handler would close the window *and* let the application drop its last window,
    /// ending the process. Destroying by hand and stopping the emission leaves the
    /// process alive with no window, which is what `--background` means.
    #[template_callback]
    fn on_close_request(&self) -> glib::Propagation {
        let imp = self.imp();
        let scanners = imp.scanners.get().expect("scanners set in `new`");
        let commands = imp.commands.get().expect("commands set in `new`");
        let lifecycle = imp.lifecycle.get().expect("lifecycle set in `new`");

        if scanners.begin_discovery_stop() {
            let _ = commands.try_send(BusCommand::StopDiscovery { quiet: true });
        }
        if lifecycle.is_background_held() {
            self.destroy();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    }

    /// One button for both directions: starts discovery when idle, stops it while it
    /// runs, and does nothing while a stop is already in flight.
    ///
    /// The `begin_*` calls are the store's own guard against a second command for a
    /// transition already under way, so the button never has to track that itself.
    #[template_callback]
    fn on_find_clicked(&self) {
        let imp = self.imp();
        let scanners = imp.scanners.get().expect("scanners set in `new`");
        let commands = imp.commands.get().expect("commands set in `new`");

        match scanners.discovery_state() {
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
        }
    }

    /// Clears the footer's selection, then switches `page_stack` to the row's name.
    ///
    /// `row-selected` fires with `None` when a list box is unselected, including the
    /// `unselect_all` its mirror below calls — so the early return is what stops the two
    /// handlers driving each other in a loop.
    #[template_callback]
    fn on_section_selected(&self, row: Option<gtk::ListBoxRow>) {
        let Some(row) = row else {
            return;
        };
        let imp = self.imp();
        imp.footer.unselect_all();
        imp.page_stack
            .set_visible_child_name(row.widget_name().as_str());
    }

    /// Mirror of [`Self::on_section_selected`]: clears that selection, then switches
    /// `page_stack`.
    #[template_callback]
    fn on_footer_selected(&self, row: Option<gtk::ListBoxRow>) {
        let Some(row) = row else {
            return;
        };
        let imp = self.imp();
        imp.sections.unselect_all();
        imp.page_stack
            .set_visible_child_name(row.widget_name().as_str());
    }
}

/// The window's half of the one GTK test — see [`crate::gtk_tests`].
///
/// The instantiation is the point §10 of the design doc makes: `init_template` fills
/// every `#[template_child]` in `imp` from the id of the same name in the compiled
/// `.ui`, so a child renamed or dropped in `window.blp` fails on the `new` below rather
/// than at the first `present()` on someone's machine. What follows it is the wiring
/// that only exists once there is a template instance: the `swapped` handlers, and the
/// store registrations `new` makes.
#[cfg(all(test, feature = "gtk-tests"))]
pub(crate) mod widget_checks {
    use super::*;

    pub(crate) fn run() {
        // A `GtkApplicationWindow` refuses to be added to an application that has not
        // registered — a `g_critical`, not an error a caller could see — so the test
        // application registers first. `NON_UNIQUE` because two runs of the suite must
        // not talk to each other, and registration falls back to a private application
        // when there is no session bus, so this holds on a bus-less runner too.
        let app = adw::Application::builder()
            .application_id("org.scanbus.GuiTests")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(gio::Cancellable::NONE)
            .expect("register the test application");

        let model = Rc::new(ScannerListModel::new());
        // The receiver is kept: a dropped one closes the channel, and every `try_send`
        // in this file ignores its error, so the callbacks below would still look as
        // though they had sent something.
        let (commands, sent) = async_channel::unbounded();
        let window = ScanbusWindow::new(
            &app,
            Rc::clone(&model),
            commands,
            Rc::new(AppLifecycle::default()),
        );
        let imp = window.imp();

        // `new` fills the stack `window.blp` declares empty, and then selects the
        // Scanners row. The order is what this asserts: `set_visible_child_name` for a
        // child the stack does not have yet does nothing, so a window built the other way
        // round comes up on an empty pane.
        for name in ["scanners", "profiles", "settings"] {
            assert!(
                imp.page_stack.child_by_name(name).is_some(),
                "no `{name}` page in the stack"
            );
        }
        assert_eq!(
            imp.page_stack
                .visible_child_name()
                .unwrap_or_default()
                .as_str(),
            "scanners",
            "the window should open on Scanners"
        );

        // The header bar is written by `render`, which `new` calls once. The template's
        // own label is the Idle one, so only a store that has moved says whether the
        // `connect_changed` registration is really there.
        assert_eq!(
            imp.find_button.label().unwrap_or_default().as_str(),
            "Find scanners…"
        );
        assert!(!imp.spinner.get_visible());

        model.mark_discovery_active();
        assert_eq!(imp.find_button.label().unwrap_or_default().as_str(), "Stop");
        assert!(imp.spinner.get_visible() && imp.spinner.is_spinning());

        // A stop in flight keeps the label — it says what is being waited for — and
        // refuses further clicks.
        assert!(model.begin_discovery_stop());
        assert_eq!(imp.find_button.label().unwrap_or_default().as_str(), "Stop");
        assert!(!imp.find_button.get_sensitive());

        // `on_find_clicked` through the template, which is the check that `swapped` is
        // still on the handler in the `.blp`: without it `self` would be the button and
        // the store would never be reached.
        model.mark_discovery_idle();
        assert!(imp.find_button.get_sensitive());
        imp.find_button.emit_clicked();
        assert!(
            matches!(sent.try_recv(), Ok(BusCommand::StartDiscovery)),
            "the find button should have asked the bus thread to start discovery"
        );
        assert!(matches!(model.discovery_state(), DiscoveryState::Starting));
        model.mark_discovery_idle();

        // The two sidebar list boxes: each switches the stack to the selected row's
        // widget name, and clears the other's selection.
        let settings_row = imp
            .footer
            .row_at_index(0)
            .expect("the footer's Settings row");
        imp.footer.select_row(Some(&settings_row));
        assert_eq!(
            imp.page_stack
                .visible_child_name()
                .unwrap_or_default()
                .as_str(),
            "settings"
        );
        assert!(imp.sections.selected_row().is_none());

        imp.sections.select_row(Some(&*imp.scanners_row));
        assert_eq!(
            imp.page_stack
                .visible_child_name()
                .unwrap_or_default()
                .as_str(),
            "scanners"
        );
        assert!(imp.footer.selected_row().is_none());

        // Destroyed rather than left in the application's window list, since the suite
        // goes on to instantiate more widgets. `destroy` does not emit `close-request`,
        // so this is not `on_close_request` in disguise.
        window.destroy();
    }
}
