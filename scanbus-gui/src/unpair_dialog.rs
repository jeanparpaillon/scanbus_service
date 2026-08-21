//! The unpair confirmation — the composite-template subclass `unpair-dialog.blp`
//! declares.
//!
//! §7 of the GUI design owes the one destructive action in the window a confirmation, and
//! this is it. It is built per click, as it was when `scanners.rs` assembled it by hand:
//! the body names the scanner, and the dialog exists only for the length of one answer.
//! What the port buys is that its *shape* now lives in the `.blp` with every other
//! widget's, leaving `ScannersPane::confirm_unpair` as the four values it takes.
//!
//! The command is sent from here rather than reported back to the pane. There is nothing
//! for the pane to decide once *Unpair* has been clicked, and a dialog that owns its own
//! answer cannot leave a stale callback pointing at a pane that has moved on.

use std::cell::OnceCell;

use async_channel::Sender;
use gtk::{CompositeTemplate, TemplateChild, glib};
use gtk4 as gtk;
use libadwaita::prelude::*;
use libadwaita::subclass::prelude::*;

use crate::bus::BusCommand;

mod imp {
    use super::*;

    // No `Debug`: `Sender<BusCommand>` does not derive it, for the reason `window.rs`
    // gives.
    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/org/scanbus/Gui/ui/unpair-dialog.ui")]
    pub struct UnpairDialog {
        /// Written by `new`, because it names the scanner.
        #[template_child]
        pub body: TemplateChild<gtk::Label>,
        #[template_child]
        pub cancel: TemplateChild<gtk::Button>,
        #[template_child]
        pub confirm: TemplateChild<gtk::Button>,

        /// What the confirm handler needs, as `OnceCell`s for the reason `window.rs`
        /// gives: neither is a `glib::Value`.
        pub commands: OnceCell<Sender<BusCommand>>,
        pub path: OnceCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for UnpairDialog {
        /// Must match `template $UnpairDialog` in `unpair-dialog.blp`.
        const NAME: &'static str = "UnpairDialog";
        type Type = super::UnpairDialog;
        type ParentType = gtk::Window;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            // Instance callbacks, as in `window.rs`: the `#[gtk::template_callbacks]`
            // block is on the wrapper type.
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for UnpairDialog {}
    impl WidgetImpl for UnpairDialog {}
    impl WindowImpl for UnpairDialog {}
}

glib::wrapper! {
    /// The confirmation the detail pane's *Unpair* button raises.
    pub struct UnpairDialog(ObjectSubclass<imp::UnpairDialog>)
        @extends gtk::Window, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Native,
                    gtk::Root, gtk::ShortcutManager;
}

impl UnpairDialog {
    /// Builds the confirmation for one scanner.
    ///
    /// The cells are filled here, before the dialog has been presented or returned, so
    /// the confirm handler may read them as already set — nothing can reach a callback
    /// before `new` gives the dialog back.
    pub fn new(
        parent: Option<&gtk::Window>,
        scanner_name: &str,
        path: String,
        commands: Sender<BusCommand>,
    ) -> Self {
        let dialog: Self = glib::Object::new();

        let imp = dialog.imp();
        imp.body.set_label(&format!(
            "Unpairing {scanner_name} removes its saved pairing information."
        ));
        assert!(imp.commands.set(commands).is_ok(), "commands set twice");
        assert!(imp.path.set(path).is_ok(), "path set twice");

        // Modal is in the `.blp`; the parent is not, because it is the window this
        // particular dialog was raised over.
        dialog.set_transient_for(parent);

        dialog
    }
}

/// The two handlers `unpair-dialog.blp` names, in the order the template mentions them.
///
/// Both are declared `swapped` there, which is what puts the dialog in `&self`.
#[gtk::template_callbacks]
impl UnpairDialog {
    /// Cancel sends nothing. That is the whole of what the confirmation is for.
    #[template_callback]
    fn on_unpair_cancel(&self) {
        self.close();
    }

    #[template_callback]
    fn on_unpair_confirm(&self) {
        let imp = self.imp();
        let path = imp.path.get().expect("path set in `new`").clone();
        let _ = imp
            .commands
            .get()
            .expect("commands set in `new`")
            .try_send(BusCommand::Unpair { path });
        self.close();
    }
}

/// The confirmation's half of the one GTK test — see [`crate::gtk_tests`].
///
/// A module of its own rather than a few lines inside `details.rs`'s checks, per the rule
/// [`crate::gtk_tests`] states: the assertions live next to the code they are about. The
/// dialog is constructible on its own — it takes a path and a channel, not a pane — so
/// nothing has to hand it one, which is what makes `scanner_row.rs` the exception it is.
#[cfg(all(test, feature = "gtk-tests"))]
pub(crate) mod widget_checks {
    use super::*;

    const NAME: &str = "Brother MFC-L2710DW";

    pub(crate) fn run() {
        let (commands, sent) = async_channel::unbounded();
        let path = "/org/scanbus/Scanner/mock_usb_001_002".to_owned();

        // No transient parent: `present` is never called, so there is no window for this
        // one to be modal over, and both answers are reachable without one.
        let dialog = UnpairDialog::new(None, NAME, path.clone(), commands.clone());
        assert!(
            dialog.imp().body.label().contains(NAME),
            "the body has to name the scanner, or the confirmation confirms nothing"
        );

        // Cancel first, because sending nothing is the whole of what a confirmation is
        // for: a Cancel that unpaired would still pass the check below it.
        dialog.imp().cancel.emit_clicked();
        assert!(
            sent.try_recv().is_err(),
            "Cancel must leave the scanner paired"
        );

        // Confirm, clicked through the template — the check that `swapped` is still on
        // that handler: without it `self` would be the button, and neither the path nor
        // the channel would be reachable. A second dialog, because the first has closed.
        let dialog = UnpairDialog::new(None, NAME, path.clone(), commands);
        dialog.imp().confirm.emit_clicked();
        assert!(
            matches!(sent.try_recv(), Ok(BusCommand::Unpair { path: asked }) if asked == path),
            "Unpair should have asked the bus thread to unpair this scanner"
        );
    }
}
