//! The option editor: one row per `OptionsSchema` entry, and no option key named here.
//!
//! `Profile1.OptionsSchema` (API §6) exists so that a client does not have to know that
//! `image.format` is one of `jpeg|jpg|png` or that `quality` is `1..=100`. This module is
//! where that pays off: [`OptionsEditor::render`] walks the decoded schema and asks
//! [`build_row`] for a widget per `type`, so a profile gains an option when the daemon
//! publishes one and the GUI is not recompiled around it.
//!
//! The rule that follows is the one worth stating: **nothing in this file may branch on an
//! option key or a profile kind.** A special case for `output_folder` here would be the
//! hand-written editor the schema was added to delete, and it would drift from the daemon
//! the first time the table in `scanbus-daemon/src/profiles/options.rs` changed.
//!
//! # A type this version has no widget for is shown, not hidden
//!
//! [`OptionType::Unknown`] gets a read-only row naming the type and rendering the value.
//! §6 calls for exactly that, and the two alternatives are both worse: hiding the row
//! makes a daemon-side extension look like data loss, and guessing a widget offers values
//! the daemon then refuses. The same row serves a key that is *stored* but no longer
//! declared — the write below preserves it either way.
//!
//! # A write replaces the whole map, so the editor never builds one from the schema
//!
//! §6: "A write **replaces the whole map**, it does not merge". So a row change edits a
//! clone of the map the daemon last published and sends *that*, rather than collecting the
//! rows' current values. A key the schema does not declare therefore survives an unrelated
//! edit; if the daemon refuses the write for it, it says so and changes nothing, which is
//! the loud failure. Collecting from the rows would drop it silently.
//!
//! # Two levels, one editor
//!
//! [`Scope`] is the whole difference between editing `Profile1.Options` and editing a
//! `Button1.ProfileOptions` override. §6's chain is button → profile → schema default, so
//! the button editor is given the profile's own options as [`OptionsEditor::render`]'s
//! `inherited` argument and the profile editor is given nothing; both then fall back to
//! `OptionsSchema[key].default`. **Reset** removes the key rather than writing the
//! inherited value back, because those differ: a written-back value stops tracking the
//! profile (or the daemon's computed `output_folder`) from that moment on.
//!
//! # Every widget is a template, the choice between them is not
//!
//! [`OptionsEditor`] is the subclass `options-editor.blp` declares — an
//! `AdwPreferencesGroup` whose one declared child is the "No options" row, which a render
//! shows or hides rather than builds — and [`build_row`] instantiates one of the six
//! subclasses of [`crate::option_rows`] rather than assembling an Adw row and bolting a
//! suffix onto it. Everything that is *not* shape stays in this file: which of the six a
//! `type` maps to, the strings spliced into a combo's model, the bounds written to a spin
//! row's adjustment, the tooltip that names what **Reset** goes back to, and the
//! `updating` guard.
//!
//! # The store stays the truth
//!
//! A row shows what the user picked as soon as they pick it — the widget moved, and
//! freezing it would be its own lie — but nothing else in the editor moves until the
//! daemon confirms with `PropertiesChanged`. A refused write leaves the store where it was
//! and the next render puts the row back, the same deal the connection switch of
//! [`crate::buttons`] makes.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use gtk::gio;
use gtk::{CompositeTemplate, TemplateChild, glib};
use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use libadwaita::subclass::prelude::*;
use scanbus_client::profile::is_unset;
use scanbus_client::{OptionSchema, OptionType, OptionsSchema};
use scanbus_core::Value;

use crate::option_rows::{
    OptionRowChoice, OptionRowFlag, OptionRowFolder, OptionRowNumber, OptionRowReadOnly,
    OptionRowText,
};

/// Which of §6's two option maps an editor writes, and therefore what "unset" means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `Profile1.Options`. An absent key falls back to the schema's effective default.
    Profile,
    /// `Button1.ProfileOptions`. An absent key falls back to the profile's own value, and
    /// only then to the schema default.
    Button,
}

impl Scope {
    /// What a row says when the key is not in the edited map.
    fn inherited(self) -> &'static str {
        match self {
            Self::Profile => "Default",
            Self::Button => "Inherited",
        }
    }

    /// What a row says when it is.
    fn overridden(self) -> &'static str {
        match self {
            Self::Profile => "Set",
            Self::Button => "Override",
        }
    }

    /// What **Reset** goes back to — named, because it is not "the value shown".
    fn reset_tooltip(self) -> &'static str {
        match self {
            Self::Profile => "Remove this option and use the daemon's default again",
            Self::Button => "Remove this override and use the profile's option again",
        }
    }
}

/// The map an edit produced, ready for `Profile1.Options` or `Button1.ProfileOptions`.
type WriteCallback = Rc<dyn Fn(BTreeMap<String, Value>)>;

/// What every row's callback needs, shared by [`Rc`] because rows outlive a render.
struct Core {
    scope: Scope,
    schema: RefCell<OptionsSchema>,
    /// The map being edited: `Options`, or a button's `ProfileOptions`.
    stored: RefCell<BTreeMap<String, Value>>,
    /// The level below it, empty for [`Scope::Profile`].
    inherited: RefCell<BTreeMap<String, Value>>,
    /// A render is setting widget values, so their handlers are not the user talking.
    updating: Cell<bool>,
    write: RefCell<Option<WriteCallback>>,
}

impl Core {
    /// The state one editor shares with its rows, empty until the first render.
    ///
    /// A named constructor rather than the struct literal [`OptionsEditor::new`] used to
    /// hold, because the editor is a template subclass now: it is built *after*
    /// `glib::Object::new`, into the cell [`imp::OptionsEditor::core`] describes.
    fn new(scope: Scope) -> Self {
        Self {
            scope,
            schema: RefCell::new(OptionsSchema::default()),
            stored: RefCell::new(BTreeMap::new()),
            inherited: RefCell::new(BTreeMap::new()),
            updating: Cell::new(false),
            write: RefCell::new(None),
        }
    }

    /// §6's display rule down the whole chain: the edited map, then the level below it,
    /// then the schema's effective default.
    fn effective(&self, key: &str) -> Option<Value> {
        set_value(&self.stored.borrow(), key)
            .or_else(|| set_value(&self.inherited.borrow(), key))
            .or_else(|| {
                self.schema
                    .borrow()
                    .get(key)
                    .and_then(|e| e.default.clone())
            })
    }

    /// Whether *this* map holds the key — what separates an override from an inheritance.
    fn is_set(&self, key: &str) -> bool {
        set_value(&self.stored.borrow(), key).is_some()
    }

    fn commit(&self, key: &str, value: Value) {
        if self.updating.get() {
            return;
        }
        if set_value(&self.stored.borrow(), key).as_ref() == Some(&value) {
            return;
        }
        self.stored.borrow_mut().insert(key.to_owned(), value);
        self.emit();
    }

    /// **Reset**: the key goes away. Writing the inherited value back would look the same
    /// and mean something else — see the module documentation.
    fn clear(&self, key: &str) {
        if self.updating.get() || self.stored.borrow_mut().remove(key).is_none() {
            return;
        }
        self.emit();
    }

    fn emit(&self) {
        let write = self.write.borrow().clone();
        if let Some(write) = write {
            write(self.stored.borrow().clone());
        }
    }
}

mod imp {
    use super::*;

    /// No `Debug`: [`Core`] holds the write callback, which is a boxed closure.
    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/org/scanbus/Gui/ui/options-editor.ui")]
    pub struct OptionsEditor {
        /// Shown instead of the rows when a profile publishes no options at all. It is
        /// declared in the template and hidden there, so a render only ever flips it.
        #[template_child]
        pub empty: TemplateChild<adw::ActionRow>,

        pub(super) core: OnceCell<Rc<Core>>,
        pub(super) rows: RefCell<Vec<OptionRow>>,
        /// `(key, type name)` per row — the only thing a render rebuilds for.
        pub shape: RefCell<Vec<(String, String)>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for OptionsEditor {
        /// Must match `template $OptionsEditor` in `options-editor.blp`.
        const NAME: &'static str = "OptionsEditor";
        type Type = super::OptionsEditor;
        type ParentType = adw::PreferencesGroup;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl OptionsEditor {
        /// The state this editor shares with its rows.
        ///
        /// A [`OnceCell`] rather than a plain field because a template subclass is
        /// constructed by `g_object_new` and can take no argument of ours, while
        /// [`Scope`] — which of §6's two maps is being edited — is not something the
        /// editor can guess. [`super::OptionsEditor::new`] sets it immediately, so the
        /// panic below can only be reached by an editor GtkBuilder made: today nothing
        /// does, and when `options-dialog.blp`'s `$OptionsEditor editor {}` is wired up
        /// it has to say which scope it opened with rather than inherit a default that
        /// would quietly write a button's override into the profile.
        pub(super) fn core(&self) -> &Rc<Core> {
            self.core
                .get()
                .expect("an OptionsEditor GtkBuilder made has no Scope yet")
        }
    }

    impl ObjectImpl for OptionsEditor {}
    impl WidgetImpl for OptionsEditor {}
    impl PreferencesGroupImpl for OptionsEditor {}
}

glib::wrapper! {
    /// One `AdwPreferencesGroup` of option rows, rebuilt only when the schema's shape
    /// changes — the subclass `options-editor.blp` declares.
    pub struct OptionsEditor(ObjectSubclass<imp::OptionsEditor>)
        @extends adw::PreferencesGroup, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl OptionsEditor {
    pub fn new(scope: Scope) -> Self {
        let editor: Self = glib::Object::new();
        if editor.imp().core.set(Rc::new(Core::new(scope))).is_err() {
            unreachable!("nothing has asked for the core between construction and here");
        }
        editor
    }

    /// The state shared with every row this editor built.
    fn core(&self) -> &Rc<Core> {
        self.imp().core()
    }

    /// What an edited row does. Replaces any previous callback.
    pub fn connect_write<F>(&self, callback: F)
    where
        F: Fn(BTreeMap<String, Value>) + 'static,
    {
        *self.core().write.borrow_mut() = Some(Rc::new(callback));
    }

    /// Renders `stored` against `schema`, with `inherited` as the level below it.
    ///
    /// `inherited` is the profile's own `Options` for [`Scope::Button`] and empty for
    /// [`Scope::Profile`].
    pub fn render(
        &self,
        schema: &OptionsSchema,
        stored: &BTreeMap<String, Value>,
        inherited: &BTreeMap<String, Value>,
    ) {
        let core = self.core();
        *core.schema.borrow_mut() = schema.clone();
        *core.stored.borrow_mut() = stored.clone();
        *core.inherited.borrow_mut() = inherited.clone();

        self.rebuild(schema, stored);

        core.updating.set(true);
        for row in self.imp().rows.borrow().iter() {
            row.update(core);
        }
        core.updating.set(false);

        let empty = self.imp().rows.borrow().is_empty();
        self.imp().empty.set_visible(empty);
    }

    /// Rebuilds the rows when — and only when — the set of keys or one of their types
    /// changed. An option whose *value* moved keeps its widget, so a `PropertiesChanged`
    /// does not close a dropdown the user has open.
    fn rebuild(&self, schema: &OptionsSchema, stored: &BTreeMap<String, Value>) {
        let imp = self.imp();
        let wanted = shape_of(schema, stored);
        if *imp.shape.borrow() == wanted {
            return;
        }

        for row in imp.rows.borrow().iter() {
            self.remove(row.widget());
        }
        imp.rows.borrow_mut().clear();

        let mut rows = Vec::with_capacity(wanted.len());
        for (key, _) in &wanted {
            let row = match schema.get(key) {
                Some(entry) => build_row(self.core(), entry),
                // Stored, undeclared: §6's read-only row, with no schema to describe it.
                None => undeclared_row(self.core(), key),
            };
            // The rows go after `empty`, which the template added first — see
            // `options-editor.blp`.
            self.add(row.widget());
            rows.push(row);
        }

        *imp.rows.borrow_mut() = rows;
        *imp.shape.borrow_mut() = wanted;
    }
}

/// What the one GTK test needs to see of a built row, without making the rows public.
#[cfg(all(test, feature = "gtk-tests"))]
impl OptionsEditor {
    /// The keys the editor built a row for, in display order.
    pub(crate) fn keys(&self) -> Vec<String> {
        self.imp()
            .rows
            .borrow()
            .iter()
            .map(|row| row.key.clone())
            .collect()
    }

    /// Which widget the factory picked — the whole point of the schema, in one string.
    pub(crate) fn widget_kind(&self, key: &str) -> Option<&'static str> {
        self.with_row(key, |row| match &row.kind {
            RowKind::Choice { .. } => "combo",
            RowKind::Number(_) => "spin",
            RowKind::Flag(_) => "switch",
            RowKind::Text(_) => "entry",
            RowKind::Folder(_) => "folder",
            RowKind::ReadOnly(_) => "read-only",
        })
    }

    /// The "Inherited"/"Override" pill, or [`None`] when the row does not show one.
    pub(crate) fn origin(&self, key: &str) -> Option<String> {
        self.with_row(key, |row| {
            row.origin
                .is_visible()
                .then(|| row.origin.label().to_string())
        })
        .flatten()
    }

    pub(crate) fn subtitle(&self, key: &str) -> Option<String> {
        self.with_row(key, |row| match &row.kind {
            RowKind::Choice { row, .. } => row.subtitle().map(|text| text.to_string()),
            RowKind::Number(row) => row.subtitle().map(|text| text.to_string()),
            RowKind::Flag(row) => row.subtitle().map(|text| text.to_string()),
            RowKind::Text(_) => None,
            RowKind::Folder(row) => row.subtitle().map(|text| text.to_string()),
            RowKind::ReadOnly(row) => row.subtitle().map(|text| text.to_string()),
        })
        .flatten()
    }

    /// Whether **Reset** is on offer for this key.
    pub(crate) fn resettable(&self, key: &str) -> bool {
        self.with_row(key, |row| {
            row.reset
                .as_ref()
                .is_some_and(gtk::prelude::WidgetExt::is_visible)
        })
        .unwrap_or(false)
    }

    pub(crate) fn press_reset(&self, key: &str) {
        self.with_row(key, |row| {
            row.reset
                .as_ref()
                .expect("no Reset on this row")
                .emit_clicked();
        });
    }

    /// Types a number into the spin row, and reports what the row settled on — the
    /// adjustment's bounds are what turn 101 into 100.
    pub(crate) fn set_number(&self, key: &str, value: f64) -> f64 {
        self.with_row(key, |row| match &row.kind {
            RowKind::Number(row) => {
                row.set_value(value);
                row.value()
            }
            _ => panic!("{key} is not a spin row"),
        })
        .expect("no row for that key")
    }

    /// Picks a value in the dropdown by its label.
    pub(crate) fn choose(&self, key: &str, value: &str) {
        self.with_row(key, |row| match &row.kind {
            RowKind::Choice { items, row } => {
                let index = items
                    .borrow()
                    .iter()
                    .position(|item| item == value)
                    .expect("the dropdown must offer that value");
                row.set_selected(index as u32);
            }
            _ => panic!("{key} is not a dropdown"),
        });
    }

    pub(crate) fn set_flag(&self, key: &str, active: bool) {
        self.with_row(key, |row| match &row.kind {
            RowKind::Flag(row) => row.set_active(active),
            _ => panic!("{key} is not a switch"),
        });
    }

    fn with_row<T>(&self, key: &str, visit: impl FnOnce(&OptionRow) -> T) -> Option<T> {
        self.imp()
            .rows
            .borrow()
            .iter()
            .find(|row| row.key == key)
            .map(visit)
    }
}

/// Every row the editor should have, in the order it shows them: the schema's own entries
/// first, then any key the map holds that the schema no longer declares.
fn shape_of(schema: &OptionsSchema, stored: &BTreeMap<String, Value>) -> Vec<(String, String)> {
    let mut shape: Vec<(String, String)> = schema
        .iter()
        .map(|entry| (entry.key.clone(), entry.value.type_name().to_owned()))
        .collect();

    for key in stored.keys() {
        if schema.get(key).is_none() {
            shape.push((key.clone(), UNDECLARED.to_owned()));
        }
    }

    shape
}

/// The pseudo-type of a key that is set but not declared — never a `type` §6 can publish,
/// so it cannot collide with one.
const UNDECLARED: &str = "\0undeclared";

/// One option's row, plus the two suffix widgets its template declared.
struct OptionRow {
    key: String,
    kind: RowKind,
    /// "Inherited" / "Override" — which level of §6's chain the shown value came from.
    origin: gtk::Label,
    /// Absent on a read-only row: there is no widget to put the value back with.
    reset: Option<gtk::Button>,
}

enum RowKind {
    /// A closed set of strings: `values` (§6), in the order the daemon published them.
    Choice {
        row: OptionRowChoice,
        items: RefCell<Vec<String>>,
    },
    /// An integer within its published bounds — the bounds are the adjustment's, so a
    /// value outside them cannot be entered.
    Number(OptionRowNumber),
    Flag(OptionRowFlag),
    /// A string with no closed set.
    Text(OptionRowText),
    /// A path: the folder chooser §6 asks a `path` to be rendered with.
    Folder(OptionRowFolder),
    /// An unknown type, or a key the schema does not declare.
    ReadOnly(OptionRowReadOnly),
}

impl OptionRow {
    /// What is left of `finish_row` now that the six `.blp`s declare the suffix: no widget
    /// is built here. The origin pill and **Reset** come off the template, and the only
    /// two things about them this editor decides are written on the way past — what the
    /// tooltip says **Reset** goes back to, and what a click on it clears.
    fn new(core: &Rc<Core>, key: String, kind: RowKind) -> Self {
        // The click is the template's — each `.blp` names `on_option_reset`, swapped, so
        // the row is what the callback gets — and what it *does* is this closure, the one
        // thing about Reset the widget cannot know: which key, and which of §6's two maps
        // to take it out of. The five `connect_reset`s are five inherent methods on five
        // unrelated types, so there is nothing to call them through; the body is written
        // once here and stamped per arm rather than being copied five times.
        macro_rules! wire_reset {
            ($row:expr) => {{
                let button = $row.reset();
                button.set_tooltip_text(Some(core.scope.reset_tooltip()));
                let core = Rc::clone(core);
                let key = key.clone();
                $row.connect_reset(move |_| core.clear(&key));
                ($row.origin(), Some(button))
            }};
        }

        let (origin, reset) = match &kind {
            RowKind::Choice { row, .. } => wire_reset!(row),
            RowKind::Number(row) => wire_reset!(row),
            RowKind::Flag(row) => wire_reset!(row),
            RowKind::Text(row) => wire_reset!(row),
            RowKind::Folder(row) => wire_reset!(row),
            // §6's read-only row: nothing here can be written, so nothing can be cleared,
            // and its `.blp` declares no Reset to hide.
            RowKind::ReadOnly(row) => (row.origin(), None),
        };

        Self {
            key,
            kind,
            origin,
            reset,
        }
    }

    fn widget(&self) -> &adw::PreferencesRow {
        match &self.kind {
            RowKind::Choice { row, .. } => row.upcast_ref(),
            RowKind::Number(row) => row.upcast_ref(),
            RowKind::Flag(row) => row.upcast_ref(),
            RowKind::Text(row) => row.upcast_ref(),
            RowKind::Folder(row) => row.upcast_ref(),
            RowKind::ReadOnly(row) => row.upcast_ref(),
        }
    }

    /// Pushes the store's value into the widget. Called inside `updating`, so the change
    /// handlers this fires are ignored.
    fn update(&self, core: &Core) {
        let value = core.effective(&self.key);
        let set = core.is_set(&self.key);

        match &self.kind {
            RowKind::Choice { row, items } => {
                let wanted = choices(core, &self.key, value.as_ref());
                if *items.borrow() != wanted {
                    // The model is the template's, so a render splices this render's
                    // strings into it rather than hanging a second one on the row.
                    let model = row.items();
                    model.splice(
                        0,
                        model.n_items(),
                        &wanted.iter().map(String::as_str).collect::<Vec<_>>(),
                    );
                    *items.borrow_mut() = wanted.clone();
                }
                let shown = value.as_ref().and_then(Value::as_str).unwrap_or_default();
                if let Some(index) = wanted.iter().position(|item| item == shown) {
                    row.set_selected(index as u32);
                }
            }
            RowKind::Number(row) => {
                if let Some(number) = value.as_ref().and_then(integer) {
                    row.set_value(number as f64);
                }
            }
            RowKind::Flag(row) => {
                row.set_active(value.as_ref().and_then(Value::as_bool) == Some(true))
            }
            RowKind::Text(row) => {
                row.set_text(value.as_ref().and_then(Value::as_str).unwrap_or_default())
            }
            RowKind::Folder(row) => {
                let path = value.as_ref().and_then(Value::as_str).unwrap_or_default();
                row.set_subtitle(&shorten_path(path));
            }
            RowKind::ReadOnly(row) => {
                row.set_subtitle(&read_only_subtitle(core, &self.key, value.as_ref()));
            }
        }

        self.origin.set_label(if set {
            core.scope.overridden()
        } else {
            core.scope.inherited()
        });
        // A row that is only ever inherited says nothing; naming the level is what makes
        // an override readable, and a wall of "Default" pills would drown it.
        self.origin.set_visible(set || core.scope == Scope::Button);

        if let Some(reset) = &self.reset {
            reset.set_visible(set);
        }
    }
}

/// The dropdown's items: the declared values, plus whatever is actually set when the
/// daemon no longer offers it. Dropping the odd one out would silently propose a change
/// the user never made the next time any row is edited.
fn choices(core: &Core, key: &str, value: Option<&Value>) -> Vec<String> {
    let schema = core.schema.borrow();
    let mut items = match schema.get(key).map(|entry| &entry.value) {
        Some(OptionType::Str { values }) => values.clone(),
        _ => Vec::new(),
    };

    if let Some(shown) = value.and_then(Value::as_str)
        && !items.iter().any(|item| item == shown)
    {
        items.push(shown.to_owned());
    }

    items
}

fn read_only_subtitle(core: &Core, key: &str, value: Option<&Value>) -> String {
    let rendered = value.map(render_value).unwrap_or_default();
    let schema = core.schema.borrow();
    match schema.get(key) {
        Some(entry) => format!(
            "{rendered} — this version has no editor for a \"{}\" option",
            entry.value.type_name()
        ),
        None => format!("{rendered} — this daemon no longer declares this option"),
    }
}

/// One schema entry's widget. The `match` is the whole factory.
fn build_row(core: &Rc<Core>, entry: &OptionSchema) -> OptionRow {
    let key = entry.key.clone();
    let title = humanize_key(&key);
    let subtitle = entry.description.clone();

    let kind = match &entry.value {
        OptionType::Str { values } if !values.is_empty() => {
            let row = OptionRowChoice::new();
            row.set_title(&title);
            row.set_subtitle(&subtitle);
            {
                let core = Rc::clone(core);
                let key = key.clone();
                row.connect_changed(move |row| {
                    let Some(item) = row.items().string(row.selected()) else {
                        return;
                    };
                    core.commit(&key, Value::Str(item.to_string()));
                });
            }
            RowKind::Choice {
                row,
                items: RefCell::new(Vec::new()),
            }
        }
        OptionType::Integer { min, max } => {
            let row = OptionRowNumber::new();
            row.set_title(&title);
            row.set_subtitle(&subtitle);
            // Before the handler is installed, not after: the template declares the
            // adjustment at zero, so writing the bounds moves the spin button's value —
            // and a row is built outside the `updating` guard, where a commit would go
            // out as if the user had typed the bound.
            let (low, high) = bounds(*min, *max);
            row.adjustment().configure(low, low, high, 1.0, 10.0, 0.0);
            {
                let core = Rc::clone(core);
                let key = key.clone();
                row.connect_changed(move |row| {
                    core.commit(&key, Value::I64(row.value() as i64));
                });
            }
            RowKind::Number(row)
        }
        OptionType::Boolean => {
            let row = OptionRowFlag::new();
            row.set_title(&title);
            row.set_subtitle(&subtitle);
            {
                let core = Rc::clone(core);
                let key = key.clone();
                row.connect_changed(move |row| {
                    core.commit(&key, Value::Bool(row.is_active()));
                });
            }
            RowKind::Flag(row)
        }
        OptionType::Path => {
            let row = OptionRowFolder::new();
            row.set_title(&title);
            row.set_tooltip_text(Some(&subtitle));
            {
                let core = Rc::clone(core);
                let key = key.clone();
                // `Choose…` is the template's button and `on_choose_folder` its callback;
                // the dialog is here because it needs the value the row currently shows
                // and the map to write back, neither of which a widget knows.
                row.connect_choose(move |row| {
                    choose_folder(&row.chooser(), &core, &key);
                });
            }
            RowKind::Folder(row)
        }
        // A `string` with no closed set, and every type this version cannot type.
        OptionType::Str { .. } => {
            let row = OptionRowText::new();
            row.set_title(&title);
            row.set_tooltip_text(Some(&subtitle));
            {
                let core = Rc::clone(core);
                let key = key.clone();
                row.connect_applied(move |row| {
                    core.commit(&key, Value::Str(row.text().to_string()));
                });
            }
            RowKind::Text(row)
        }
        OptionType::Unknown { .. } => {
            let row = OptionRowReadOnly::new();
            row.set_title(&title);
            row.set_tooltip_text(Some(&subtitle));
            RowKind::ReadOnly(row)
        }
    };

    OptionRow::new(core, key, kind)
}

/// The row for a key the map holds and the schema does not describe. The same subclass the
/// `Unknown` arm builds, and — since the suffix is the template's now — the same pill: an
/// undeclared key used to be the one row whose origin label was built and never parented.
fn undeclared_row(core: &Rc<Core>, key: &str) -> OptionRow {
    let row = OptionRowReadOnly::new();
    row.set_title(&humanize_key(key));
    OptionRow::new(core, key.to_owned(), RowKind::ReadOnly(row))
}

fn choose_folder(button: &gtk::Button, core: &Rc<Core>, key: &str) {
    let dialog = gtk::FileDialog::new();
    dialog.set_title("Choose a folder");
    dialog.set_modal(true);

    let current = core.effective(key);
    if let Some(path) = current.as_ref().and_then(Value::as_str)
        && !path.is_empty()
    {
        dialog.set_initial_folder(Some(&gio::File::for_path(path)));
    }

    let window = button.root().and_downcast::<gtk::Window>();
    let core = Rc::clone(core);
    let key = key.to_owned();
    dialog.select_folder(window.as_ref(), None::<&gio::Cancellable>, move |result| {
        // A cancelled chooser is the common outcome and not an error worth a toast.
        if let Ok(folder) = result
            && let Some(path) = folder.path()
        {
            core.commit(&key, Value::Str(path.to_string_lossy().into_owned()));
        }
    });
}

/// The adjustment a bound produces. A half-open range stays half-open: §6 publishes
/// `min`/`max` together, but inventing the missing half would refuse values the daemon
/// accepts.
///
/// `f64` is what `AdwSpinRow` takes, so the open end is 2^53 — past that an integer is no
/// longer exactly representable, and a spin row that silently rounds is worse than one
/// that stops.
fn bounds(min: Option<i128>, max: Option<i128>) -> (f64, f64) {
    const LIMIT: i128 = 1 << 53;
    let low = min.unwrap_or(-LIMIT).clamp(-LIMIT, LIMIT);
    let high = max.unwrap_or(LIMIT).clamp(low, LIMIT);
    (low as f64, high as f64)
}

/// The value a map holds for `key`, or [`None`] when §6 treats it as unset.
fn set_value(options: &BTreeMap<String, Value>, key: &str) -> Option<Value> {
    options.get(key).filter(|value| !is_unset(value)).cloned()
}

fn integer(value: &Value) -> Option<i128> {
    match value {
        Value::U64(number) => Some(i128::from(*number)),
        Value::I64(number) => Some(i128::from(*number)),
        _ => None,
    }
}

/// `output_folder` reads as *Output folder*.
pub fn humanize_key(key: &str) -> String {
    let spaced = key.replace(['_', '-'], " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

pub fn render_value(value: &Value) -> String {
    match value {
        Value::Bool(value) => (if *value { "Yes" } else { "No" }).to_owned(),
        Value::U64(value) => value.to_string(),
        Value::I64(value) => value.to_string(),
        Value::F64(value) => value.to_string(),
        Value::Str(value) => shorten_path(value),
        Value::Array(items) => items
            .iter()
            .map(render_value)
            .collect::<Vec<_>>()
            .join(", "),
        Value::Dict(_) => "…".to_owned(),
    }
}

/// `/home/jean/Documents/Scans` reads as `Documents/Scans`, as in the mockup. Anything
/// that is not under `$HOME` is shown verbatim — an absolute path elsewhere is exactly
/// the detail the user needs.
pub fn shorten_path(value: &str) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return value.to_owned();
    };
    let home = home.to_string_lossy().into_owned();
    if home.is_empty() {
        return value.to_owned();
    }

    value
        .strip_prefix(&format!("{}/", home.trim_end_matches('/')))
        .map_or_else(|| value.to_owned(), str::to_owned)
}

/// The schema fixtures every widget check shares, built from the wire shape §6 describes
/// rather than from a Rust literal — a fixture assembled by hand could not catch a decode
/// this crate gets wrong.
#[cfg(all(test, feature = "gtk-tests"))]
pub(crate) mod fixtures {
    use std::collections::HashMap;

    use zbus::zvariant::{OwnedValue, Value as ZValue};

    use super::*;
    use crate::store::Dict;

    pub(crate) fn owned(value: ZValue<'_>) -> OwnedValue {
        OwnedValue::try_from(value).expect("the fixture must be a D-Bus value")
    }

    /// One `OptionsSchema` payload, entry by entry.
    pub(crate) fn schema_dict(entries: Vec<(&str, Vec<(&str, OwnedValue)>)>) -> Dict {
        entries
            .into_iter()
            .map(|(key, fields)| {
                let fields: HashMap<String, OwnedValue> = fields
                    .into_iter()
                    .map(|(name, value)| (name.to_owned(), value))
                    .collect();
                (key.to_owned(), owned(ZValue::from(fields)))
            })
            .collect()
    }

    pub(crate) fn schema(entries: Vec<(&str, Vec<(&str, OwnedValue)>)>) -> OptionsSchema {
        OptionsSchema::decode(&schema_dict(entries)).expect("the fixture must decode")
    }

    /// `/org/scanbus/profile/image` exactly as API §6 prints it: a closed set, a bounded
    /// integer and a computed path.
    pub(crate) fn image_schema() -> OptionsSchema {
        schema(vec![
            (
                "format",
                vec![
                    ("type", owned(ZValue::from("string"))),
                    ("default", owned(ZValue::from("jpeg"))),
                    ("values", owned(ZValue::from(vec!["jpeg", "jpg", "png"]))),
                    (
                        "description",
                        owned(ZValue::from("Encoding of the written page files")),
                    ),
                ],
            ),
            (
                "quality",
                vec![
                    ("type", owned(ZValue::from("integer"))),
                    ("default", owned(ZValue::from(90u64))),
                    ("min", owned(ZValue::from(1u64))),
                    ("max", owned(ZValue::from(100u64))),
                    (
                        "description",
                        owned(ZValue::from("JPEG quality; ignored when format is png")),
                    ),
                ],
            ),
            (
                "output_folder",
                vec![
                    ("type", owned(ZValue::from("path"))),
                    (
                        "default",
                        owned(ZValue::from("/home/user/Pictures/scanbus/image")),
                    ),
                    (
                        "description",
                        owned(ZValue::from("Directory the pages are written to")),
                    ),
                ],
            ),
        ])
    }

    /// `/org/scanbus/profile/document` — where the boolean of §6 actually lives.
    pub(crate) fn document_schema() -> OptionsSchema {
        schema(vec![
            (
                "format",
                vec![
                    ("type", owned(ZValue::from("string"))),
                    ("default", owned(ZValue::from("pdf"))),
                    ("values", owned(ZValue::from(vec!["pdf"]))),
                    ("description", owned(ZValue::from("Output document format"))),
                ],
            ),
            (
                "multi_page",
                vec![
                    ("type", owned(ZValue::from("boolean"))),
                    ("default", owned(ZValue::from(true))),
                    (
                        "description",
                        owned(ZValue::from(
                            "Assemble every page into one PDF instead of one PDF per page",
                        )),
                    ),
                ],
            ),
            (
                "output_folder",
                vec![
                    ("type", owned(ZValue::from("path"))),
                    (
                        "default",
                        owned(ZValue::from("/home/user/Documents/scanbus/document")),
                    ),
                    (
                        "description",
                        owned(ZValue::from("Directory the document is written to")),
                    ),
                ],
            ),
        ])
    }
}

/// The factory's half of the one GTK test — see [`crate::gtk_tests`] for why there is
/// only one.
#[cfg(all(test, feature = "gtk-tests"))]
pub(crate) mod widget_checks {
    use std::cell::RefCell;

    use zbus::zvariant::Value as ZValue;

    use super::fixtures::{document_schema, image_schema, owned, schema};
    use super::*;

    /// Every map the editor has sent, in order.
    type Writes = Rc<RefCell<Vec<BTreeMap<String, Value>>>>;

    /// Collects what the editor sends, so an assertion can be about the map that reaches
    /// the daemon rather than about the widget that produced it.
    fn recording(scope: Scope) -> (OptionsEditor, Writes) {
        let editor = OptionsEditor::new(scope);
        let writes = Rc::new(RefCell::new(Vec::new()));
        {
            let writes = Rc::clone(&writes);
            editor.connect_write(move |options| writes.borrow_mut().push(options));
        }
        (editor, writes)
    }

    fn last(writes: &Writes) -> BTreeMap<String, Value> {
        writes.borrow().last().cloned().expect("no write was sent")
    }

    pub(crate) fn run() {
        every_row_template_binds_and_fires();
        the_editor_is_a_template_too();
        every_declared_type_gets_its_widget();
        an_unknown_type_is_read_only_and_still_listed();
        a_new_schema_entry_appears_on_its_own();
        the_widget_refuses_what_the_daemon_would();
        a_write_replaces_the_map_it_was_given();
        reset_removes_the_key_instead_of_writing_the_inherited_value();
    }

    /// All six templates, instantiated directly rather than through a schema: each one
    /// loads, binds the suffix the six `.blp`s declare, and delivers a click to the slot
    /// the factory installs on it.
    ///
    /// The checks below reach the same six rows through [`OptionsEditor::render`], where a
    /// template that stopped binding shows up as a mystery two assertions later — a
    /// missing `#[template_child]` panics inside `origin()`, and a handler line dropped
    /// from a `.blp` does not fail at all, it just stops working.
    fn every_row_template_binds_and_fires() {
        // `TemplateChild::get` panics on a child `init_template` did not fill, so reaching
        // the suffix off a row is itself the check that its template loaded and that the
        // names in the `.blp` still match the fields. The click is the other half: the
        // button's `clicked` reaches `on_option_reset`, `swapped` so the row rather than
        // the button is `&self`, and the row fires the closure `OptionRow::new` installed
        // on it. A handler line dropped from a `.blp` does not fail to compile — it stops
        // working, silently, which is what this catches.
        macro_rules! check_row {
            ($name:literal, $row:expr) => {{
                let row = $row;
                let (origin, reset) = (row.origin(), row.reset());
                assert!(
                    origin.is_ancestor(&row),
                    "{}: the origin pill is not in the row — which is what `undeclared_row` \
                     used to get wrong, building a label and never parenting it",
                    $name
                );
                assert!(
                    reset.is_ancestor(&row),
                    "{}: Reset is not in the row",
                    $name
                );
                assert!(
                    !reset.is_visible(),
                    "{}: Reset is on offer before any render decided the value is set at \
                     this level",
                    $name
                );

                let fired = Rc::new(Cell::new(false));
                {
                    let fired = Rc::clone(&fired);
                    row.connect_reset(move |_| fired.set(true));
                }
                reset.emit_clicked();
                assert!(fired.get(), "{}: Reset reached no slot", $name);
                row
            }};
        }

        check_row!("choice", OptionRowChoice::new());
        check_row!("number", OptionRowNumber::new());
        check_row!("flag", OptionRowFlag::new());
        check_row!("text", OptionRowText::new());
        let folder = check_row!("folder", OptionRowFolder::new());

        // The sixth row has no Reset — nothing on it can be written — so it gets the half
        // of the check that is about the pill. An undeclared key is drawn with this row,
        // and before the port it was the one row whose origin label was built and never
        // parented, which is the failure this line would have caught.
        let read_only = OptionRowReadOnly::new();
        assert!(
            read_only.origin().is_ancestor(&read_only),
            "read-only: the origin pill is not in the row"
        );

        // The chooser is the other click the factory hangs a closure on, and the only one
        // no other check can reach: pressing it for real opens a `gtk::FileDialog`.
        let chose = Rc::new(Cell::new(false));
        {
            let chose = Rc::clone(&chose);
            folder.connect_choose(move |_| chose.set(true));
        }
        folder.chooser().emit_clicked();
        assert!(
            chose.get(),
            "Choose… reached no slot, so the file dialog would never open"
        );
    }

    /// The editor is a template as well, and its "No options" row is declared in it and
    /// hidden there — a render only ever flips it.
    fn the_editor_is_a_template_too() {
        let (editor, _) = recording(Scope::Profile);
        let empty = editor.imp().empty.get();
        assert!(
            !empty.is_visible(),
            "the empty-state row must start hidden: an editor that has not rendered yet \
             does not know whether the profile has options"
        );

        editor.render(&schema(vec![]), &BTreeMap::new(), &BTreeMap::new());
        assert!(editor.keys().is_empty());
        assert!(
            empty.is_visible(),
            "a profile that publishes no options must say so rather than show a blank group"
        );

        editor.render(&image_schema(), &BTreeMap::new(), &BTreeMap::new());
        assert!(
            !empty.is_visible(),
            "the empty-state row must go once there are rows to show"
        );
    }

    /// Acceptance: the exported profiles render as a combo, a spin row, a switch and a
    /// folder chooser — and nothing in [`super`] names one of these keys.
    fn every_declared_type_gets_its_widget() {
        let (editor, writes) = recording(Scope::Profile);

        editor.render(&image_schema(), &BTreeMap::new(), &BTreeMap::new());
        assert_eq!(editor.widget_kind("format"), Some("combo"));
        assert_eq!(editor.widget_kind("quality"), Some("spin"));
        assert_eq!(editor.widget_kind("output_folder"), Some("folder"));

        editor.render(&document_schema(), &BTreeMap::new(), &BTreeMap::new());
        assert_eq!(editor.widget_kind("multi_page"), Some("switch"));

        // The switch writes the boolean the schema says it is, not the string a
        // hand-written editor would have to remember to spell right.
        editor.set_flag("multi_page", false);
        assert_eq!(last(&writes).get("multi_page"), Some(&Value::Bool(false)));

        // An unset key shows the daemon's effective default, not a blank — §6's whole
        // reason for publishing a resolved `output_folder`.
        assert_eq!(
            editor.subtitle("output_folder").as_deref(),
            Some("/home/user/Documents/scanbus/document")
        );
        assert!(
            !editor.resettable("output_folder"),
            "there is nothing to reset a value the profile never set"
        );

        // A string with no closed set is a text field, not a dropdown of nothing.
        editor.render(
            &schema(vec![(
                "subject",
                vec![
                    ("type", owned(ZValue::from("string"))),
                    ("default", owned(ZValue::from(""))),
                ],
            )]),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert_eq!(editor.widget_kind("subject"), Some("entry"));
    }

    /// Acceptance: an unknown `type` renders read-only rather than crashing or hiding the
    /// row. The same row carries a key the schema stopped declaring.
    fn an_unknown_type_is_read_only_and_still_listed() {
        let (editor, _) = recording(Scope::Profile);
        let schema = schema(vec![
            (
                "languages",
                vec![
                    ("type", owned(ZValue::from("string-list"))),
                    ("default", owned(ZValue::from(vec!["eng"]))),
                    ("description", owned(ZValue::from("OCR languages"))),
                ],
            ),
            (
                "format",
                vec![
                    ("type", owned(ZValue::from("string"))),
                    ("default", owned(ZValue::from("pdf"))),
                    ("values", owned(ZValue::from(vec!["pdf"]))),
                ],
            ),
        ]);
        let stored = BTreeMap::from([("retired".to_owned(), Value::U64(3))]);

        editor.render(&schema, &stored, &BTreeMap::new());

        assert_eq!(editor.widget_kind("languages"), Some("read-only"));
        assert!(
            editor
                .subtitle("languages")
                .is_some_and(|text| text.contains("string-list")),
            "the row must name the type it cannot edit: {:?}",
            editor.subtitle("languages")
        );
        assert!(
            !editor.resettable("languages"),
            "a row with no editor must not offer to change the value either"
        );
        assert_eq!(
            editor.widget_kind("format"),
            Some("combo"),
            "one odd entry must not cost the rest of the page"
        );

        // A stored key the schema no longer declares is listed, not silently dropped.
        assert_eq!(editor.widget_kind("retired"), Some("read-only"));
        assert!(
            editor
                .subtitle("retired")
                .is_some_and(|text| text.contains("no longer declares"))
        );
    }

    /// Acceptance: an option the daemon adds shows up with no GUI change outside this
    /// module — here, an entry appended to a schema the editor has already rendered.
    fn a_new_schema_entry_appears_on_its_own() {
        fn quality() -> Vec<(&'static str, zbus::zvariant::OwnedValue)> {
            vec![
                ("type", owned(ZValue::from("integer"))),
                ("default", owned(ZValue::from(90u64))),
                ("min", owned(ZValue::from(1u64))),
                ("max", owned(ZValue::from(100u64))),
            ]
        }

        let (editor, _) = recording(Scope::Profile);
        editor.render(
            &schema(vec![("quality", quality())]),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert_eq!(editor.keys(), ["quality"]);

        // The daemon grew an option. Nothing below this line knows its name.
        let grown = schema(vec![
            ("quality", quality()),
            (
                "deskew",
                vec![
                    ("type", owned(ZValue::from("boolean"))),
                    ("default", owned(ZValue::from(false))),
                    ("description", owned(ZValue::from("Straighten each page"))),
                ],
            ),
        ]);

        editor.render(&grown, &BTreeMap::new(), &BTreeMap::new());
        assert_eq!(editor.keys(), ["deskew", "quality"]);
        assert_eq!(editor.widget_kind("deskew"), Some("switch"));
    }

    /// Acceptance: `quality = 101` is refused by the widget. The bound comes from the
    /// schema, so the daemon's `1..=100` is the spin row's range by construction.
    fn the_widget_refuses_what_the_daemon_would() {
        let (editor, writes) = recording(Scope::Profile);
        editor.render(&image_schema(), &BTreeMap::new(), &BTreeMap::new());

        assert_eq!(editor.set_number("quality", 101.0), 100.0);
        assert_eq!(
            last(&writes).get("quality"),
            Some(&Value::I64(100)),
            "the write must carry what the widget settled on"
        );

        assert_eq!(editor.set_number("quality", 0.0), 1.0);
        assert_eq!(last(&writes).get("quality"), Some(&Value::I64(1)));
    }

    /// §6: a write replaces the whole map. So one edited row sends every other key back
    /// with it — including one the schema does not declare, which the daemon then refuses
    /// loudly instead of the GUI dropping it quietly.
    fn a_write_replaces_the_map_it_was_given() {
        let (editor, writes) = recording(Scope::Profile);
        let stored = BTreeMap::from([
            ("quality".to_owned(), Value::U64(60)),
            ("retired".to_owned(), Value::Str("kept".to_owned())),
        ]);

        editor.render(&image_schema(), &stored, &BTreeMap::new());
        editor.choose("format", "png");

        let written = last(&writes);
        assert_eq!(written.get("format"), Some(&Value::Str("png".to_owned())));
        assert_eq!(written.get("quality"), Some(&Value::U64(60)));
        assert_eq!(
            written.get("retired"),
            Some(&Value::Str("kept".to_owned())),
            "a key this version cannot edit is not a key it may delete"
        );

        // Re-picking what is already selected is not an edit.
        let before = writes.borrow().len();
        editor.choose("format", "png");
        assert_eq!(writes.borrow().len(), before);
    }

    /// Acceptance: **Reset** removes the key. Writing the inherited value back would show
    /// the same thing and stop tracking the profile from then on.
    fn reset_removes_the_key_instead_of_writing_the_inherited_value() {
        let (editor, writes) = recording(Scope::Button);
        let inherited = BTreeMap::from([
            ("format".to_owned(), Value::Str("png".to_owned())),
            (
                "output_folder".to_owned(),
                Value::Str("/srv/profile".to_owned()),
            ),
        ]);
        let stored = BTreeMap::from([
            (
                "output_folder".to_owned(),
                Value::Str("/srv/this-key".to_owned()),
            ),
            ("quality".to_owned(), Value::U64(60)),
        ]);

        editor.render(&image_schema(), &stored, &inherited);

        assert_eq!(editor.origin("output_folder").as_deref(), Some("Override"));
        assert_eq!(
            editor.origin("format").as_deref(),
            Some("Inherited"),
            "a key the profile sets and the button does not is inherited, not set here"
        );
        assert!(editor.resettable("output_folder"));
        assert!(!editor.resettable("format"));
        assert_eq!(
            editor.subtitle("output_folder").as_deref(),
            Some("/srv/this-key")
        );

        editor.press_reset("output_folder");

        let written = last(&writes);
        assert!(
            !written.contains_key("output_folder"),
            "Reset wrote the inherited value back instead of removing the key: {written:?}"
        );
        assert_eq!(
            written.get("quality"),
            Some(&Value::U64(60)),
            "resetting one key must not clear the others"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_keys_read_as_prose() {
        assert_eq!(humanize_key("output_folder"), "Output folder");
        assert_eq!(humanize_key("format"), "Format");
        assert_eq!(humanize_key("multi-page"), "Multi page");
        assert_eq!(humanize_key(""), "");
    }

    #[test]
    fn values_render_for_a_reader_not_for_a_parser() {
        assert_eq!(render_value(&Value::Bool(true)), "Yes");
        assert_eq!(render_value(&Value::U64(300)), "300");
        assert_eq!(
            render_value(&Value::Array(vec![
                Value::Str("eng".to_owned()),
                Value::Str("fra".to_owned()),
            ])),
            "eng, fra"
        );
    }

    #[test]
    fn a_path_under_home_loses_the_home_prefix() {
        let previous = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", "/home/scanbus");
        }

        assert_eq!(
            shorten_path("/home/scanbus/Documents/Scans"),
            "Documents/Scans"
        );
        assert_eq!(shorten_path("/srv/scans"), "/srv/scans");
        assert_eq!(shorten_path("/home/scanbus"), "/home/scanbus");

        match previous {
            Some(home) => unsafe { std::env::set_var("HOME", home) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// §6 emits `min` and `max` together; a daemon that publishes one is not a reason to
    /// invent the other, and a spin row cannot represent an integer it would round.
    #[test]
    fn a_half_open_range_stays_half_open() {
        assert_eq!(bounds(Some(1), Some(100)), (1.0, 100.0));

        let (low, high) = bounds(Some(1), None);
        assert_eq!(low, 1.0);
        assert!(
            high >= f64::from(u32::MAX),
            "an absent max is not a max of 1"
        );

        let (low, high) = bounds(None, Some(100));
        assert!(low < 0.0);
        assert_eq!(high, 100.0);

        // Wider than an f64 can count in, clamped rather than rounded.
        let (low, high) = bounds(Some(i128::from(i64::MIN)), Some(i128::from(u64::MAX)));
        assert_eq!(low, -((1i64 << 53) as f64));
        assert_eq!(high, (1i64 << 53) as f64);
    }

    /// Whitespace is unset (§6), so it never counts as a value set at this level — which
    /// is what decides whether a row reads *Override* and offers **Reset**.
    #[test]
    fn an_empty_string_is_not_a_value_this_level_holds() {
        let options = BTreeMap::from([
            ("output_folder".to_owned(), Value::Str("   ".to_owned())),
            ("format".to_owned(), Value::Str("png".to_owned())),
        ]);

        assert_eq!(set_value(&options, "output_folder"), None);
        assert_eq!(
            set_value(&options, "format"),
            Some(Value::Str("png".to_owned()))
        );
        assert_eq!(set_value(&options, "quality"), None);
    }
}
