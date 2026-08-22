//! The *Profiles* view — `docs/scanbus-gnome-gui.md` §3.
//!
//! There is no page here for `image` and none for `document`. The view is a loop over the
//! `Profile1` objects the store holds, and every widget inside one comes from
//! [`crate::options`] walking that profile's `OptionsSchema`. That is the whole point of
//! 10.14 and 10.16: the daemon's option table (`scanbus-daemon/src/profiles/options.rs`)
//! is published as a property, so adding an option there makes a row appear here without
//! a line changing in the GUI.
//!
//! `profiles-page.blp` holds the two groups whose count is fixed — the one shown when the
//! daemon exports no profile, and the one listing advertised kinds with no object — and
//! nothing else: the editors between them are one per exported profile, so they are built
//! here.
//!
//! # A kind with no object is said out loud
//!
//! `Manager1.GetProfileTypes` and the exported `Profile1` objects are not the same list.
//! API §6 is explicit that `email` and `ocr` "are design targets, not yet exported as
//! `Profile1` objects", and this daemon narrows `GetProfileTypes` to what it can run
//! rather than advertising all four — but a client cannot assume either shape. So the view
//! is built from the objects, and every advertised kind without one gets a row saying so.
//!
//! Leaving them out instead would be the same bug in two directions: a user who cannot
//! find `ocr` has no way to tell "this build has no OCR" from "the GUI forgot to show it",
//! and a daemon that starts exporting `ocr` tomorrow would need a GUI change to reveal it.

use std::cell::{OnceCell, RefCell};
use std::collections::BTreeMap;

use async_channel::Sender;
use gtk::glib;
use gtk::{CompositeTemplate, TemplateChild};
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use libadwaita::subclass::prelude::*;
use scanbus_client::convert;
use scanbus_core::ProfileKind;

use crate::bus::BusCommand;
use crate::options::{OptionsEditor, Scope};
use crate::scanners::humanize_profile;
use crate::store::ProfileEntry;

/// The private instance struct GObject owns for the page; [`ProfilesPage`] is the handle
/// `ScanbusWindow` holds.
mod imp {
    use super::*;

    /// No `Debug`: the bus channel and the editors are plain Rust types that do not
    /// derive it.
    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/org/scanbus/Gui/ui/profiles-page.ui")]
    pub struct ProfilesPage {
        /// The daemon is not running, or exports no profile at all. Declared hidden, so
        /// a render only ever flips it.
        #[template_child]
        pub absent: TemplateChild<adw::PreferencesGroup>,
        /// Declared with no rows: there is one per advertised kind the daemon exports no
        /// object for, and that count comes from the bus.
        #[template_child]
        pub unimplemented: TemplateChild<adw::PreferencesGroup>,

        /// One editor per exported `Profile1`, keyed the way the store keys them.
        pub(super) editors: RefCell<Vec<(ProfileKind, OptionsEditor)>>,
        pub(super) unimplemented_rows: RefCell<Vec<adw::ActionRow>>,

        /// A `OnceCell` for the reason `options.rs` gives at length: a template subclass
        /// is constructed by `g_object_new` and can be handed no argument of ours, while
        /// the channel an edited row writes down is not something the page can guess.
        pub(super) commands: OnceCell<Sender<BusCommand>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ProfilesPage {
        /// Must match `template $ProfilesPage` in `profiles-page.blp`.
        const NAME: &'static str = "ProfilesPage";
        type Type = super::ProfilesPage;
        type ParentType = adw::PreferencesPage;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for ProfilesPage {}
    impl WidgetImpl for ProfilesPage {}
    impl PreferencesPageImpl for ProfilesPage {}
}

glib::wrapper! {
    /// The Profiles page, built from `profiles-page.blp` and written from the store.
    pub struct ProfilesPage(ObjectSubclass<imp::ProfilesPage>)
        @extends adw::PreferencesPage, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl ProfilesPage {
    pub fn new(commands: Sender<BusCommand>) -> Self {
        let page: Self = glib::Object::new();

        // Empty cell: the object was built on the line above and this is its only
        // constructor, so the `set` cannot fail — see the note in `window.rs`.
        assert!(
            page.imp().commands.set(commands).is_ok(),
            "commands set twice"
        );

        page
    }

    /// The channel an edited row writes down.
    fn commands(&self) -> &Sender<BusCommand> {
        self.imp()
            .commands
            .get()
            .expect("a ProfilesPage GtkBuilder made has no bus channel yet")
    }

    /// Renders the whole view from the store.
    ///
    /// `advertised` is `Manager1.GetProfileTypes` as it arrived — strings, not
    /// [`ProfileKind`]s, because a version that adds a kind must be reported rather than
    /// dropped at the parse.
    pub fn render(&self, profiles: &BTreeMap<ProfileKind, ProfileEntry>, advertised: &[String]) {
        self.rebuild(profiles);

        for (kind, editor) in self.imp().editors.borrow().iter() {
            let Some(entry) = profiles.get(kind) else {
                continue;
            };
            editor.render(
                &entry.schema,
                &convert::from_dict(&entry.options).unwrap_or_default(),
                &BTreeMap::new(),
            );
        }

        self.render_unimplemented(profiles, advertised);
        self.imp().absent.set_visible(profiles.is_empty());
    }

    /// Adds and removes editors when the exported profiles change, and only then — a
    /// profile whose `Options` moved keeps its widgets, so a `PropertiesChanged` does not
    /// scroll the view or close a dropdown.
    fn rebuild(&self, profiles: &BTreeMap<ProfileKind, ProfileEntry>) {
        let imp = self.imp();
        let wanted: Vec<ProfileKind> = profiles.keys().copied().collect();
        if imp
            .editors
            .borrow()
            .iter()
            .map(|(kind, _)| *kind)
            .eq(wanted.iter().copied())
        {
            return;
        }

        for (_, editor) in imp.editors.borrow().iter() {
            self.remove(editor);
        }
        imp.editors.borrow_mut().clear();

        // Both informational groups are children of the page before the first render —
        // that is what putting them in the template means — so `add` would append every
        // editor *below* the unimplemented-kinds group. `insert` places each one at an
        // explicit index instead: `absent` is the group at 0, so the editors take 1..n
        // in the store's `BTreeMap` order and `unimplemented` is pushed along ahead of
        // them, staying last.
        //
        // `adw_preferences_page_insert` is libadwaita 1.8; the crate asks for `v1_9`.
        // Without it the same order costs a remove and a re-add of `unimplemented`
        // around the loop, which is what this did while the feature level was 1.4.
        let mut editors = Vec::with_capacity(wanted.len());
        for (position, kind) in wanted.into_iter().enumerate() {
            let editor = OptionsEditor::new(Scope::Profile);
            editor.set_title(&humanize_profile(kind.as_str()));
            editor.set_description(Some(
                "Every scan that runs this profile uses these options unless a button \
                 overrides one.",
            ));

            {
                let commands = self.commands().clone();
                editor.connect_write(move |options| {
                    let _ = commands.try_send(BusCommand::SetProfileOptions { kind, options });
                });
            }

            // The editors already inserted shift the next one along, hence the running
            // index rather than a constant one.
            self.insert(&editor, 1 + position as i32);
            editors.push((kind, editor));
        }

        *imp.editors.borrow_mut() = editors;
    }

    fn render_unimplemented(
        &self,
        profiles: &BTreeMap<ProfileKind, ProfileEntry>,
        advertised: &[String],
    ) {
        let imp = self.imp();
        let missing: Vec<&String> = advertised
            .iter()
            .filter(|name| {
                !name
                    .parse::<ProfileKind>()
                    .is_ok_and(|kind| profiles.contains_key(&kind))
            })
            .collect();

        for row in imp.unimplemented_rows.borrow().iter() {
            imp.unimplemented.remove(row);
        }
        imp.unimplemented_rows.borrow_mut().clear();

        let mut rows = Vec::with_capacity(missing.len());
        for name in missing {
            let row = adw::ActionRow::builder()
                .title(humanize_profile(name))
                .subtitle("Advertised by the service, but not exported as an object yet")
                .build();
            let tag = gtk::Label::new(Some("Not implemented"));
            tag.add_css_class("dim-label");
            tag.add_css_class("caption");
            tag.set_valign(gtk::Align::Center);
            row.add_suffix(&tag);
            imp.unimplemented.add(&row);
            rows.push(row);
        }

        imp.unimplemented.set_visible(!rows.is_empty());
        *imp.unimplemented_rows.borrow_mut() = rows;
    }
}

/// The Profiles view's half of the one GTK test — see [`crate::gtk_tests`].
#[cfg(all(test, feature = "gtk-tests"))]
pub(crate) mod widget_checks {
    use scanbus_client::OptionsSchema;

    use super::*;
    use crate::options::fixtures::{document_schema, image_schema};

    fn profile(kind: ProfileKind, schema: OptionsSchema) -> ProfileEntry {
        ProfileEntry {
            kind,
            options: crate::store::Dict::new(),
            schema,
        }
    }

    fn exported() -> BTreeMap<ProfileKind, ProfileEntry> {
        BTreeMap::from([
            (
                ProfileKind::Image,
                profile(ProfileKind::Image, image_schema()),
            ),
            (
                ProfileKind::Document,
                profile(ProfileKind::Document, document_schema()),
            ),
        ])
    }

    fn advertised(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// The page's groups, in the order they are drawn.
    ///
    /// `AdwPreferencesPage` publishes no list of its groups, and where it keeps them —
    /// a box, a clamp, a scroller around both — is its template's business and has
    /// changed across releases. So this walks the page's descendants in tree order,
    /// which is draw order, and stops at each group: a group's own children are rows,
    /// never groups, so nothing below one can be missed. The result is the sequence
    /// `adw_preferences_page_insert` indexes into.
    fn drawn(page: &ProfilesPage) -> Vec<adw::PreferencesGroup> {
        fn walk(widget: &gtk::Widget, found: &mut Vec<adw::PreferencesGroup>) {
            let mut next = widget.first_child();
            while let Some(child) = next {
                next = child.next_sibling();
                match child.downcast::<adw::PreferencesGroup>() {
                    Ok(group) => found.push(group),
                    Err(other) => walk(&other, found),
                }
            }
        }

        let mut found = Vec::new();
        walk(page.upcast_ref(), &mut found);
        found
    }

    pub(crate) fn run() {
        let (commands, sent) = async_channel::unbounded();
        let page = ProfilesPage::new(commands);
        let imp = page.imp();

        // §2 advertises four kinds; this daemon exports two. Both halves are shown.
        page.render(
            &exported(),
            &advertised(&["image", "document", "email", "ocr"]),
        );

        {
            let editors = imp.editors.borrow();
            assert_eq!(
                editors.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
                [ProfileKind::Image, ProfileKind::Document],
                "the view is the objects that exist, in the store's order"
            );

            // Order, and not only membership. Both fixed groups are on the page before
            // the first render, so an editor added rather than inserted lands *below*
            // the unimplemented-kinds group — the regression 10.23 is written around.
            let drawn = drawn(&page);
            let at = |group: &adw::PreferencesGroup| {
                drawn
                    .iter()
                    .position(|candidate| candidate == group)
                    .expect("a group the page does not hold")
            };
            assert_eq!(at(&imp.absent.get()), 0, "the absent group is not first");
            assert_eq!(
                editors
                    .iter()
                    .map(|(_, editor)| at(editor.upcast_ref()))
                    .collect::<Vec<_>>(),
                [1, 2],
                "the editors are not between the two fixed groups"
            );
            assert_eq!(
                at(&imp.unimplemented.get()),
                drawn.len() - 1,
                "the unimplemented-kinds group is not last"
            );

            // Acceptance: the whole widget vocabulary of §6, with no profile named in
            // `profiles.rs` or `options.rs`.
            let image = &editors[0].1;
            assert_eq!(image.widget_kind("format"), Some("combo"));
            assert_eq!(image.widget_kind("quality"), Some("spin"));
            assert_eq!(image.widget_kind("output_folder"), Some("folder"));

            let document = &editors[1].1;
            assert_eq!(document.widget_kind("multi_page"), Some("switch"));

            // A profile's own option is set, not inherited from anywhere else, so no
            // pill until the user sets one.
            assert_eq!(image.origin("quality"), None);
            assert!(!image.resettable("quality"));

            image.set_number("quality", 75.0);
        }

        match sent.try_recv() {
            Ok(BusCommand::SetProfileOptions { kind, options }) => {
                assert_eq!(
                    kind,
                    ProfileKind::Image,
                    "the write went to another profile"
                );
                assert_eq!(options.get("quality"), Some(&scanbus_core::Value::I64(75)));
            }
            other => panic!("expected a Profile1.Options write, got {other:?}"),
        }

        // The kinds with no object say so rather than going missing.
        let rows = imp.unimplemented_rows.borrow();
        assert!(imp.unimplemented.is_visible());
        assert_eq!(
            rows.iter()
                .map(|row| row.title().to_string())
                .collect::<Vec<_>>(),
            ["E-mail", "OCR"]
        );
        drop(rows);

        // A daemon that advertises only what it exports has nothing to disclaim.
        page.render(&exported(), &advertised(&["image", "document"]));
        assert!(imp.unimplemented_rows.borrow().is_empty());
        assert!(!imp.unimplemented.is_visible());
        assert!(!imp.absent.is_visible());

        // A profile that stops being exported takes its editor with it.
        page.render(&BTreeMap::new(), &advertised(&["image", "document"]));
        assert!(imp.editors.borrow().is_empty());
        assert!(imp.absent.is_visible());
        assert_eq!(imp.unimplemented_rows.borrow().len(), 2);
    }
}
