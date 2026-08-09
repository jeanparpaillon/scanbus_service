//! [`Button1`]: what the firmware imposes, and what the host assigns.
//!
//! §5 of [`scanbus-dbus-api.md`] splits the properties of a physical key in two, and the
//! split is the interface's whole point. `Index`, `DeviceLabel` and `LabelConfigurable`
//! are read-only facts about the device; `Label`, `Profile` and `ProfileOptions` are host
//! state. A configuration UI needs both halves to tell "renaming this key is
//! unavailable" apart from "renaming this key is broken" — on the Brother MFC in front of
//! us `LabelConfigurable=false` for all four keys, and offering a rename would produce a
//! write that silently did nothing.
//!
//! # The device label and the profile are allowed to disagree
//!
//! Key 2 is engraved "Scan to OCR" and the host may assign it `document`. §5 says so
//! explicitly, and nothing here forces the two to match: flagging the inconsistency is a
//! UI client's job, and a daemon that "helpfully" refused the write would make a
//! documented configuration impossible. The only thing a `Profile` write is checked
//! against is [`ProfileKind::supported`] — what `GetProfileTypes` advertises.
//!
//! # A write reaches the vendor config before it reaches the property
//!
//! Writing `Profile` "makes the service rewrite the relevant backend's configuration and
//! reload it" (§5), which is
//! [`set_button_mapping`](ScannerBackend::set_button_mapping). That call is **fallible
//! for reasons outside this daemon** — on Brother it rewrites a file under `/opt` that
//! may not be writable (5.4) — so the order is: validate, call the backend, and only then
//! move the property. The other order leaves the daemon and the device disagreeing about
//! what key 2 does, with a client that has been told the write succeeded.
//!
//! The state therefore lives behind a [`Mutex`] that is **held across the backend call**.
//! That is what makes "the property moved" and "the config was rewritten" one step: two
//! clients writing `Profile` at the same moment otherwise interleave into a property that
//! records one write and a config file that records the other. zbus dispatches a `&self`
//! property setter under a read lock on the interface, so several can genuinely be in
//! flight at once — see [`crate::dbus::scanner`] on why nothing here takes `&mut self`.
//!
//! # What a property setter cannot say
//!
//! §8 names this daemon's errors, and `org.scanbus.Error.UnsupportedProfile` is the name
//! for a profile this pipeline will not run. **A property setter cannot answer with it**:
//! zbus serves `org.freedesktop.DBus.Properties` itself and its `Set` returns
//! [`zbus::fdo::Error`], so the reply's name comes from that closed set whatever a setter
//! returns ([`crate::dbus::error`] argues it at length, and `Scanner1.DefaultProfile` has
//! the same limit). A refused `Profile` write is therefore
//! `org.freedesktop.DBus.Error.InvalidArgs` with a message naming the profile and the
//! list it had to be in.
//!
//! # There is no backend seam for a label
//!
//! [`ScannerBackend`] can write a key→profile mapping and nothing else, so a `Label`
//! write on a device that accepts one (`LabelConfigurable=true`, the HP touchscreens of
//! §5) is recorded here and not pushed to the device. That is honest for the hardware
//! this iteration has — every device it supports reports `false` — and the backend that
//! reports `true` is the one that adds the seam.
//!
//! # An assignment does not yet survive a restart
//!
//! The host's half lives in the [`ButtonInfo`] below and nowhere else, so a
//! `systemctl --user restart` loses it — the same limit
//! [`MemoryPairingStore`](crate::store::MemoryPairingStore) has for pairings, and for the
//! same reason: [`PairingStore`](scanbus_core::PairingStore) holds a
//! [`ScannerInfo`](scanbus_core::ScannerInfo), which is the *backend's* half of a scanner
//! and deliberately carries no assignments. Giving them a file is [4.1], whose title says
//! so; what it needs from here is already in place, since [`ButtonInfo`] serialises whole
//! and is the unit a restore would hand to [`Button1::new`].
//!
//! Until then the honest reading of a `Profile` write is that it took effect on the
//! *device* — the vendor config is rewritten and outlives us — and that the daemon's copy
//! of it does not.
//!
//! [4.1]: https://github.com/jeanparpaillon/scanbus_service/issues/16
//! [`scanbus-dbus-api.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-dbus-api.md

use std::collections::BTreeMap;
use std::sync::Arc;

use scanbus_core::{BackendError, ButtonInfo, ProfileKind, ScannerBackend, ScannerId, Value};
use tokio::sync::Mutex;
use tracing::{debug, info, instrument};
use zbus::fdo;

use crate::dbus::convert::{self, Dict};

/// The `org.scanbus.Button1` object of §5, at
/// `/org/scanbus/scanner/{id}/button/{n}`.
///
/// One per entry of the device's physical menu. The registry
/// ([`crate::scanners`]) creates them from `Capabilities.buttons.count` alongside the
/// scanner object and takes them down with it.
pub struct Button1 {
    /// The scanner this key belongs to — the `{id}` of this object's path, and what the
    /// backend is told a mapping is for.
    scanner: ScannerId,
    /// The backend that found the scanner, and therefore the one whose configuration a
    /// `Profile` write rewrites.
    ///
    /// The same instance the listener was started against, not whatever the daemon's
    /// backend list ranks first: a mapping written into the wrong vendor's config is a
    /// key that does nothing.
    backend: Arc<dyn ScannerBackend>,
    /// Everything about the key, read-only half included.
    ///
    /// One lock rather than one per property, because a `Profile` write reads
    /// `ProfileOptions` in the same breath — the backend takes both — and two locks would
    /// let a `ProfileOptions` write slip between the read and the call.
    info: Mutex<ButtonInfo>,
}

impl Button1 {
    /// A key of `scanner`, configured through `backend`.
    pub fn new(scanner: ScannerId, backend: Arc<dyn ScannerBackend>, info: ButtonInfo) -> Self {
        Self {
            scanner,
            backend,
            info: Mutex::new(info),
        }
    }

    /// The profile assigned to this key, as a [`ProfileKind`] rather than as the
    /// `""`-for-unassigned string the property carries.
    ///
    /// Named apart from the `Profile` getter below, which the interface macro owns. This
    /// is what [2.6] reads off the matching object when it stamps a job: the first term of
    /// the precedence order in
    /// [`ButtonEvent::profile`](crate::listeners::ButtonEvent::profile), read at the
    /// moment of the press rather than when the listener started.
    ///
    /// [2.6]: https://github.com/jeanparpaillon/scanbus_service/issues/10
    pub async fn assigned_profile(&self) -> Option<ProfileKind> {
        self.info.lock().await.profile
    }

    /// The options recorded for this key, for [2.6] to run the profile with.
    ///
    /// Named apart from the `ProfileOptions` getter below, which the interface macro owns.
    ///
    /// [2.6]: https://github.com/jeanparpaillon/scanbus_service/issues/10
    pub async fn assigned_options(&self) -> BTreeMap<String, Value> {
        self.info.lock().await.profile_options.clone()
    }

    /// Position in the physical menu — also the `{n}` of this object's path.
    ///
    /// Named apart from the `Index` getter below, which the interface macro owns.
    pub async fn position(&self) -> u32 {
        self.info.lock().await.index
    }
}

#[zbus::interface(name = "org.scanbus.Button1")]
impl Button1 {
    /// Position in the physical menu, 0-based.
    ///
    /// `const` because it is the `{n}` element of the object path: a key whose index
    /// changed would be a different object, not a property change on this one.
    #[zbus(property(emits_changed_signal = "const"))]
    async fn index(&self) -> u32 {
        self.position().await
    }

    /// The label the firmware imposes, e.g. `"Scan to E-mail"`.
    ///
    /// Empty when the device exposes none of its own — a generic touchscreen, and today
    /// every device this daemon can enumerate, since the backend seam reports
    /// `buttons.count` and no labels to go with it.
    #[zbus(property)]
    async fn device_label(&self) -> String {
        self.info.lock().await.device_label.clone()
    }

    /// Whether the host can push a custom label to the device.
    ///
    /// `false` on classic Brother devices with fixed keys, which is what stops a UI
    /// offering a rename that would silently fail (§9).
    #[zbus(property)]
    async fn label_configurable(&self) -> bool {
        self.info.lock().await.label_configurable
    }

    /// The label actually displayed: what the host assigned, or the firmware's own.
    ///
    /// The fallback is what makes `LabelConfigurable=false` coherent rather than a lie —
    /// with nothing assignable, `Label` simply mirrors `DeviceLabel`.
    #[zbus(property)]
    async fn label(&self) -> String {
        self.info.lock().await.effective_label().to_owned()
    }

    /// Assigns the label displayed on the device, where the device allows it.
    ///
    /// `""` clears the assignment, which is how a client gets back to mirroring
    /// `DeviceLabel` once it has written something else.
    ///
    /// # Errors
    ///
    /// `org.freedesktop.DBus.Error.PropertyReadOnly` when `LabelConfigurable=false`. §5
    /// allows a write to be "ignored or error out" there, and erroring is the only one of
    /// the two a UI can act on: a silently ignored write is indistinguishable from a
    /// device that accepted it and re-rendered later. The property keeps mirroring
    /// `DeviceLabel`.
    #[zbus(property)]
    async fn set_label(&self, value: String) -> fdo::Result<()> {
        let mut info = self.info.lock().await;

        if !info.label_configurable {
            return Err(fdo::Error::PropertyReadOnly(format!(
                "key {} of {} has a fixed label: LabelConfigurable is false, so Label \
                 mirrors DeviceLabel ({:?})",
                info.index, self.scanner, info.device_label
            )));
        }

        info.label = (!value.is_empty()).then_some(value);
        debug!(id = %self.scanner, index = info.index, label = %info.effective_label(), "label set");

        Ok(())
    }

    /// The profile this key triggers, `""` when it is unassigned (§5).
    #[zbus(property)]
    async fn profile(&self) -> String {
        ProfileKind::optional_as_str(self.assigned_profile().await).to_owned()
    }

    /// Assigns the profile this key triggers, rewriting the backend's configuration.
    ///
    /// Not checked against `DeviceLabel`: assigning `document` to a key the firmware calls
    /// "Scan to OCR" is explicitly allowed (§5) and is a UI's business to warn about, not
    /// this daemon's to refuse.
    ///
    /// `""` clears the key, and clearing reaches the vendor configuration as a removal
    /// rather than being kept to ourselves — otherwise the device goes on triggering a
    /// scan the daemon no longer has a mapping for.
    ///
    /// # Errors
    ///
    /// `org.freedesktop.DBus.Error.InvalidArgs` for a profile outside `GetProfileTypes`,
    /// raised **before the backend is called** so that a rejected value cannot have
    /// touched a config file. §8's name for this is
    /// `org.scanbus.Error.UnsupportedProfile`, which a property setter cannot carry — see
    /// the module documentation.
    ///
    /// Whatever the backend refused the rewrite with, otherwise. The property then still
    /// reads its previous value, because it is only moved once the backend has confirmed.
    #[zbus(property)]
    #[instrument(level = "info", skip_all, fields(id = %self.scanner, profile = %value))]
    async fn set_profile(&self, value: String) -> fdo::Result<()> {
        let profile = parse_profile(&value)?;
        let mut info = self.info.lock().await;

        // Under the lock, so the options written into the config are the ones this key has
        // when the call lands — not the ones a concurrent `ProfileOptions` write left.
        self.backend
            .set_button_mapping(&self.scanner, info.index, profile, &info.profile_options)
            .await
            .map_err(backend_refused)?;

        info.profile = profile;
        info!(index = info.index, "key assigned");

        Ok(())
    }

    /// Options for this profile on this key, e.g. a different output folder per key (§5).
    #[zbus(property)]
    async fn profile_options(&self) -> Dict {
        convert::dict(
            self.info
                .lock()
                .await
                .profile_options
                .iter()
                .map(|(key, value)| (key.clone(), value)),
        )
    }

    /// Replaces this key's options, rewriting the backend's configuration.
    ///
    /// Replaces rather than merges: `a{sv}` has no spelling for "remove this key", so a
    /// merging setter would leave an option a client had set impossible to unset.
    ///
    /// # Errors
    ///
    /// `org.freedesktop.DBus.Error.InvalidArgs` for a value the model cannot carry — a
    /// struct, a file descriptor, a dictionary keyed by anything but a string (see
    /// [`crate::dbus::convert`]) — raised before the backend is called.
    ///
    /// Whatever the backend refused the rewrite with, otherwise; the options then still
    /// read their previous value.
    #[zbus(property)]
    #[instrument(level = "info", skip_all, fields(id = %self.scanner))]
    async fn set_profile_options(&self, value: Dict) -> fdo::Result<()> {
        let options = convert::from_dict(&value).map_err(|error| {
            fdo::Error::InvalidArgs(format!("ProfileOptions rejected: {error}"))
        })?;

        let mut info = self.info.lock().await;

        // Sent with the profile the key currently has: the backend's mapping is one
        // record, and rewriting it with a profile of `None` would clear the key as a side
        // effect of setting an option on it.
        self.backend
            .set_button_mapping(&self.scanner, info.index, info.profile, &options)
            .await
            .map_err(backend_refused)?;

        info.profile_options = options;
        info!(
            index = info.index,
            options = info.profile_options.len(),
            "key options set"
        );

        Ok(())
    }
}

/// Reads a `Profile` write, refusing anything this daemon will not run.
///
/// The check `GetProfileTypes` promises, on the same list
/// ([`ProfileKind::supported`]) the manager answers with. `email` and `ocr` parse — a
/// config UI may already hold one, and §6 lists all four — and are refused here, which is
/// the distinction [`ProfileKind::is_supported`] exists for.
fn parse_profile(value: &str) -> fdo::Result<Option<ProfileKind>> {
    let supported = ProfileKind::supported();

    let refused = || {
        let names: Vec<&str> = supported.iter().map(|kind| kind.as_str()).collect();
        fdo::Error::InvalidArgs(format!(
            "profile {value:?} is not one this daemon runs; GetProfileTypes is [{}] and \
             \"\" clears the key",
            names.join(", ")
        ))
    };

    let profile = ProfileKind::parse_optional(value).map_err(|_| refused())?;
    match profile {
        Some(kind) if !supported.contains(&kind) => Err(refused()),
        profile => Ok(profile),
    }
}

/// Turns a backend's refusal into the reply a property setter can carry.
///
/// Not the general mapping — that is [2.7]'s, and it maps every [`BackendError`] onto the
/// named errors of §8. A `Set` reply cannot carry those names at all (see the module
/// documentation), so what is left is the choice between the standard names, and it is
/// worth making: `UnknownObject` says the scanner went away, `NotSupported` says the
/// device has nothing this profile maps onto, and `Failed` carries everything else with
/// the backend's own message — which for Brother is the path of the file it could not
/// write.
///
/// [2.7]: https://github.com/jeanparpaillon/scanbus_service/issues/11
fn backend_refused(error: BackendError) -> fdo::Error {
    let detail = error.to_string();

    match error {
        BackendError::UnknownScanner(_) => fdo::Error::UnknownObject(detail),
        BackendError::UnsupportedProfile(_) | BackendError::Unsupported { .. } => {
            fdo::Error::NotSupported(detail)
        }
        _ => fdo::Error::Failed(detail),
    }
}

#[cfg(test)]
mod tests {
    use scanbus_core::backend::mock::{ButtonMapping, MockBackend, MockCall};

    use super::*;

    fn scanner_id() -> ScannerId {
        ScannerId::from_backend("mock", "usb:001:002").unwrap()
    }

    /// Key 2 of the Brother MFC of §5: engraved "Scan to OCR", label not configurable.
    fn ocr_key(backend: &MockBackend) -> Button1 {
        Button1::new(
            scanner_id(),
            Arc::new(backend.clone()),
            ButtonInfo::new(2, "Scan to OCR", false),
        )
    }

    fn options(folder: &str) -> Dict {
        convert::dict([("output_folder".to_owned(), &Value::Str(folder.to_owned()))])
    }

    /// The §5 case that has to keep working: the firmware calls it OCR and the host
    /// assigns `document`. Refusing this would make a documented setup impossible.
    #[tokio::test]
    async fn a_profile_may_disagree_with_the_device_label() {
        let backend = MockBackend::new();
        let handle = backend.handle();
        let key = ocr_key(&backend);

        key.set_profile("document".to_owned()).await.unwrap();

        assert_eq!(key.profile().await, "document");
        assert_eq!(key.device_label().await, "Scan to OCR");
        assert_eq!(
            handle.button_mapping(&scanner_id(), 2).map(|m| m.profile),
            Some(ProfileKind::Document)
        );
    }

    /// Checklist: a profile outside `GetProfileTypes` is refused *before* the backend is
    /// asked, so a rejected write cannot have touched a config file.
    #[tokio::test]
    async fn an_unrunnable_profile_never_reaches_the_backend() {
        let backend = MockBackend::new();
        let handle = backend.handle();
        let key = ocr_key(&backend);

        // `ocr` parses — a config UI can hold one — and this pipeline will not run it.
        let error = key.set_profile("ocr".to_owned()).await.unwrap_err();
        assert!(matches!(error, fdo::Error::InvalidArgs(_)), "{error:?}");
        assert!(error.to_string().contains("image, document"), "{error}");

        // And one that is not a profile name at all.
        assert!(key.set_profile("pdf".to_owned()).await.is_err());

        assert_eq!(key.profile().await, "");
        assert!(handle.calls().is_empty(), "{:?}", handle.calls());
    }

    /// Acceptance: a backend that refuses the rewrite leaves the property where it was.
    #[tokio::test]
    async fn a_refused_write_leaves_the_property_alone() {
        let backend = MockBackend::new();
        let handle = backend.handle();
        let key = ocr_key(&backend);

        key.set_profile("image".to_owned()).await.unwrap();
        handle.fail_button_mapping(BackendError::Other(
            "/opt/brother/scanner/brscan5/brsanenetdevice4.cfg is read-only".to_owned(),
        ));

        let error = key.set_profile("document".to_owned()).await.unwrap_err();
        assert!(matches!(error, fdo::Error::Failed(_)), "{error:?}");
        assert!(error.to_string().contains("read-only"), "{error}");
        assert_eq!(key.profile().await, "image");

        // The same holds for the options half of the write.
        handle.succeed_button_mapping();
        key.set_profile_options(options("~/Scans/invoices"))
            .await
            .unwrap();
        handle.fail_button_mapping(BackendError::Other("still read-only".to_owned()));
        assert!(
            key.set_profile_options(options("~/Elsewhere"))
                .await
                .is_err()
        );
        assert_eq!(
            key.assigned_options().await.get("output_folder"),
            Some(&Value::Str("~/Scans/invoices".to_owned()))
        );
    }

    /// Checklist: `Label` on a fixed-key device errors, and goes on mirroring
    /// `DeviceLabel`.
    #[tokio::test]
    async fn a_fixed_label_refuses_a_write_and_keeps_mirroring() {
        let backend = MockBackend::new();
        let key = ocr_key(&backend);

        let error = key.set_label("Invoices".to_owned()).await.unwrap_err();
        assert!(
            matches!(error, fdo::Error::PropertyReadOnly(_)),
            "{error:?}"
        );
        assert_eq!(key.label().await, "Scan to OCR");
        assert!(!key.label_configurable().await);
    }

    /// A device that does accept a label records it, and `""` gets back to the firmware's.
    #[tokio::test]
    async fn a_configurable_label_is_assigned_and_cleared() {
        let backend = MockBackend::new();
        let key = Button1::new(
            scanner_id(),
            Arc::new(backend),
            ButtonInfo::new(0, "Scan 1", true),
        );

        assert_eq!(key.label().await, "Scan 1");
        key.set_label("Invoices".to_owned()).await.unwrap();
        assert_eq!(key.label().await, "Invoices");

        key.set_label(String::new()).await.unwrap();
        assert_eq!(key.label().await, "Scan 1");
    }

    /// The options are written with the profile the key already has, and clearing the
    /// profile removes the entry rather than leaving one behind.
    #[tokio::test]
    async fn options_travel_with_the_profile_and_clearing_removes_the_entry() {
        let backend = MockBackend::new();
        let handle = backend.handle();
        let key = ocr_key(&backend);

        key.set_profile("document".to_owned()).await.unwrap();
        key.set_profile_options(options("~/Scans/invoices"))
            .await
            .unwrap();

        assert_eq!(
            handle.button_mapping(&scanner_id(), 2),
            Some(ButtonMapping {
                profile: ProfileKind::Document,
                options: BTreeMap::from([(
                    "output_folder".to_owned(),
                    Value::Str("~/Scans/invoices".to_owned()),
                )]),
            })
        );

        key.set_profile(String::new()).await.unwrap();
        assert_eq!(key.profile().await, "");
        assert_eq!(handle.button_mapping(&scanner_id(), 2), None);
        assert_eq!(
            handle.calls().last(),
            Some(&MockCall::SetButtonMapping {
                scanner: scanner_id(),
                button_index: 2,
                profile: None,
            })
        );
    }

    /// An option the model cannot carry is refused before the backend is asked.
    #[tokio::test]
    async fn an_unrepresentable_option_never_reaches_the_backend() {
        let backend = MockBackend::new();
        let handle = backend.handle();
        let key = ocr_key(&backend);

        let structure = zbus::zvariant::OwnedValue::try_from(zbus::zvariant::Value::from((
            1u32,
            "two".to_owned(),
        )))
        .unwrap();

        let error = key
            .set_profile_options(Dict::from([("size".to_owned(), structure)]))
            .await
            .unwrap_err();
        assert!(matches!(error, fdo::Error::InvalidArgs(_)), "{error:?}");
        assert!(key.assigned_options().await.is_empty());
        assert!(handle.calls().is_empty(), "{:?}", handle.calls());
    }
}
