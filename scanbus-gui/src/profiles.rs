//! The *Profiles* view — `docs/scanbus-gnome-gui.md` §3.
//!
//! There is no page here for `image` and none for `document`. The view is a loop over the
//! `Profile1` objects the store holds, and every widget inside one comes from
//! [`crate::options`] walking that profile's `OptionsSchema`. That is the whole point of
//! 10.14 and 10.16: the daemon's option table (`scanbus-daemon/src/profiles/options.rs`)
//! is published as a property, so adding an option there makes a row appear here without
//! a line changing in the GUI.
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

use std::cell::RefCell;
use std::collections::BTreeMap;

use async_channel::Sender;
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use scanbus_client::convert;
use scanbus_core::ProfileKind;

use crate::bus::BusCommand;
use crate::options::{OptionsEditor, Scope};
use crate::scanners::humanize_profile;
use crate::store::ProfileEntry;

pub struct ProfilesPage {
    root: gtk::ScrolledWindow,
    page: adw::PreferencesPage,
    /// One editor per exported `Profile1`, keyed the way the store keys them.
    editors: RefCell<Vec<(ProfileKind, OptionsEditor)>>,
    unimplemented: adw::PreferencesGroup,
    unimplemented_rows: RefCell<Vec<adw::ActionRow>>,
    /// The daemon is not running, or exports no profile at all.
    absent: adw::PreferencesGroup,
    commands: Sender<BusCommand>,
}

impl ProfilesPage {
    pub fn new(commands: Sender<BusCommand>) -> Self {
        let page = adw::PreferencesPage::new();

        let absent = adw::PreferencesGroup::builder().title("Profiles").build();
        absent.add(
            &adw::ActionRow::builder()
                .title("No profiles")
                .subtitle(
                    "The scanbus service exports no profile objects. Start it from \
                     Settings, then come back.",
                )
                .build(),
        );
        absent.set_visible(false);
        page.add(&absent);

        let unimplemented = adw::PreferencesGroup::builder()
            .title("Not implemented yet")
            .description(
                "The service reports these profile types but exports no object for them, \
                 so they have no options to edit.",
            )
            .build();
        unimplemented.set_visible(false);
        page.add(&unimplemented);

        let root = gtk::ScrolledWindow::new();
        root.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        root.set_child(Some(&page));

        Self {
            root,
            page,
            editors: RefCell::new(Vec::new()),
            unimplemented,
            unimplemented_rows: RefCell::new(Vec::new()),
            absent,
            commands,
        }
    }

    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.root
    }

    /// Renders the whole view from the store.
    ///
    /// `advertised` is `Manager1.GetProfileTypes` as it arrived — strings, not
    /// [`ProfileKind`]s, because a version that adds a kind must be reported rather than
    /// dropped at the parse.
    pub fn render(&self, profiles: &BTreeMap<ProfileKind, ProfileEntry>, advertised: &[String]) {
        self.rebuild(profiles);

        for (kind, editor) in self.editors.borrow().iter() {
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
        self.absent.set_visible(profiles.is_empty());
    }

    /// Adds and removes editors when the exported profiles change, and only then — a
    /// profile whose `Options` moved keeps its widgets, so a `PropertiesChanged` does not
    /// scroll the view or close a dropdown.
    fn rebuild(&self, profiles: &BTreeMap<ProfileKind, ProfileEntry>) {
        let wanted: Vec<ProfileKind> = profiles.keys().copied().collect();
        if self
            .editors
            .borrow()
            .iter()
            .map(|(kind, _)| *kind)
            .eq(wanted.iter().copied())
        {
            return;
        }

        for (_, editor) in self.editors.borrow().iter() {
            self.page.remove(editor.widget());
        }
        self.editors.borrow_mut().clear();

        // The informational groups were added first and have to end up last, so they are
        // lifted out and put back around the profiles.
        self.page.remove(&self.unimplemented);

        let mut editors = Vec::with_capacity(wanted.len());
        for kind in wanted {
            let editor = OptionsEditor::new(Scope::Profile);
            editor.set_title(&humanize_profile(kind.as_str()));
            editor.set_description(Some(
                "Every scan that runs this profile uses these options unless a button \
                 overrides one.",
            ));

            {
                let commands = self.commands.clone();
                editor.connect_write(move |options| {
                    let _ = commands.try_send(BusCommand::SetProfileOptions { kind, options });
                });
            }

            self.page.add(editor.widget());
            editors.push((kind, editor));
        }

        self.page.add(&self.unimplemented);
        *self.editors.borrow_mut() = editors;
    }

    fn render_unimplemented(
        &self,
        profiles: &BTreeMap<ProfileKind, ProfileEntry>,
        advertised: &[String],
    ) {
        let missing: Vec<&String> = advertised
            .iter()
            .filter(|name| {
                !name
                    .parse::<ProfileKind>()
                    .is_ok_and(|kind| profiles.contains_key(&kind))
            })
            .collect();

        for row in self.unimplemented_rows.borrow().iter() {
            self.unimplemented.remove(row);
        }
        self.unimplemented_rows.borrow_mut().clear();

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
            self.unimplemented.add(&row);
            rows.push(row);
        }

        self.unimplemented.set_visible(!rows.is_empty());
        *self.unimplemented_rows.borrow_mut() = rows;
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

    pub(crate) fn run() {
        let (commands, sent) = async_channel::unbounded();
        let page = ProfilesPage::new(commands);

        // §2 advertises four kinds; this daemon exports two. Both halves are shown.
        page.render(
            &exported(),
            &advertised(&["image", "document", "email", "ocr"]),
        );

        {
            let editors = page.editors.borrow();
            assert_eq!(
                editors.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
                [ProfileKind::Image, ProfileKind::Document],
                "the view is the objects that exist, in the store's order"
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
        let rows = page.unimplemented_rows.borrow();
        assert!(page.unimplemented.is_visible());
        assert_eq!(
            rows.iter()
                .map(|row| row.title().to_string())
                .collect::<Vec<_>>(),
            ["E-mail", "OCR"]
        );
        drop(rows);

        // A daemon that advertises only what it exports has nothing to disclaim.
        page.render(&exported(), &advertised(&["image", "document"]));
        assert!(page.unimplemented_rows.borrow().is_empty());
        assert!(!page.unimplemented.is_visible());
        assert!(!page.absent.is_visible());

        // A profile that stops being exported takes its editor with it.
        page.render(&BTreeMap::new(), &advertised(&["image", "document"]));
        assert!(page.editors.borrow().is_empty());
        assert!(page.absent.is_visible());
        assert_eq!(page.unimplemented_rows.borrow().len(), 2);
    }
}
