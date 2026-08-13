//! Recognising an HP scanner in an mDNS record.
//!
//! `_uscan._tcp` and `_scanner._tcp` are vendor-neutral service types — the Brother
//! MFC-J5335DW on the development LAN advertises both — so a browse hit is a candidate,
//! not a device. Two filters turn one into the other, in the order HPLIP applies them:
//!
//! 1. the record must name HP: a manufacturer TXT key if the device publishes one
//!    (`usb_MFG`, `mfg`), otherwise the leading word of `ty=` or of the instance name;
//! 2. the model, normalized the way `base/models.py:320` normalizes it, must resolve in
//!    `models.dat` with a non-zero `scan-type` — the same test `__checkFilter('scan', mq)`
//!    applies to an `hp-probe` result, and the reason a printer-only HP is not offered as
//!    a scanner.
//!
//! Step 2 is also the real vendor filter: `models.dat` is HP's own model list, so a
//! Brother cannot resolve in it whatever it calls itself. Step 1 exists to keep the
//! rejection cheap and to say *why* in the log.
//!
//! The model that comes back out is the URI spelling — `HP_OfficeJet_Pro_8610` — which is
//! what both `hp:/net/…` and `hpaio:/net/…` are built from.

// The filter lands before the discovery path that calls it, so that it can be reviewed
// against captured records on its own. Remove this with the browse wiring.
#![allow(dead_code)]

use std::collections::BTreeMap;

use mdns_sd::ServiceInfo;
use tracing::debug;

use super::ModelInfo;

/// TXT keys that state the manufacturer, most specific first. `mdns-sd` looks TXT keys
/// up case insensitively, so `usb_MFG` also matches a device that spells it `usb_mfg`.
const VENDOR_KEYS: [&str; 2] = ["usb_MFG", "mfg"];
/// TXT keys carrying the model without the manufacturer in front of it — `_scanner._tcp`
/// splits the two (`mfg=Brother`, `mdl=MFC-J5335DW`) where `_uscan._tcp` only has `ty`.
const MODEL_KEYS: [&str; 2] = ["usb_MDL", "mdl"];
/// The human-readable device name, present on both service types.
const TYPE_KEY: &str = "ty";

/// The HP model this record is for, in URI spelling, or `None` if it is not an HP
/// scanner.
///
/// `models` is [`super::load_models`]'s map. An empty one — `models.dat` unreadable —
/// rejects everything, which is deliberate: without HP's model list there is nothing
/// left that distinguishes an HP `_uscan._tcp` record from anyone else's, and claiming a
/// Brother for the hplip backend would cost the user the working eSCL path.
pub(crate) fn hp_scan_model(
    service: &ServiceInfo,
    models: &BTreeMap<String, ModelInfo>,
) -> Option<String> {
    let instance = instance_name(service);

    if !names_hp(service, instance) {
        debug!(instance, "mDNS record does not name HP; ignoring");
        return None;
    }

    let candidates = model_candidates(service, instance);
    for candidate in &candidates {
        let key = model_key(candidate);
        match models.get(&key) {
            Some(model) if model.scan_type != 0 => return Some(normalize_model_name(candidate)),
            Some(_) => debug!(instance, %key, "HP model does not scan; ignoring"),
            None => {}
        }
    }

    debug!(
        instance,
        tried = %candidates.join(", "),
        "no candidate model resolved to a scanning entry in models.dat; ignoring"
    );
    None
}

/// HPLIP's model-name normalization, ported from `base/models.py:320`.
///
/// The odd order — collapse `__` *before* turning `/` into `_` — is HPLIP's, and is kept
/// because the result has to match a `models.dat` section name character for character.
/// Each step is Python's `str.replace`, i.e. a single left-to-right pass, so `___` here
/// becomes `__` exactly as it does there rather than collapsing all the way down.
fn normalize_model_name(model: &str) -> String {
    model
        .replace(' ', "_")
        .replace("__", "_")
        .replace('~', "")
        .replace('/', "_")
        .trim_matches('_')
        .to_owned()
}

/// The `models.dat` section name for a model, matching [`super::model_key_from_uri`].
fn model_key(model: &str) -> String {
    normalize_model_name(model).to_ascii_lowercase()
}

/// The instance part of the fullname: `HP OfficeJet` out of
/// `HP OfficeJet._uscan._tcp.local.`.
fn instance_name(service: &ServiceInfo) -> &str {
    let fullname = service.get_fullname();
    fullname
        .strip_suffix(service.get_type())
        .map_or(fullname, |instance| instance.trim_end_matches('.'))
}

/// Whether this record is HP's.
///
/// A manufacturer TXT key is decisive when the device publishes one: a record that says
/// `mfg=Brother` is a Brother however its `ty` reads. Only when there is none does the
/// device name get to answer, which is the `_uscan._tcp` case — eSCL has no manufacturer
/// field, so `ty=HP OfficeJet Pro 8610` is all there is to go on.
fn names_hp(service: &ServiceInfo, instance: &str) -> bool {
    if let Some(vendor) = VENDOR_KEYS
        .iter()
        .find_map(|key| service.get_property_val_str(key))
    {
        return is_hp_vendor(vendor.trim());
    }

    let type_name = service.get_property_val_str(TYPE_KEY).unwrap_or_default();
    names_hp_device(type_name) || names_hp_device(instance)
}

/// Whether a manufacturer field names HP. `Hewlett-Packard` is what HPLIP's own device
/// IDs carry (`base/mdns.py:279`); devices in the field also use the short form.
fn is_hp_vendor(vendor: &str) -> bool {
    let vendor = vendor.to_ascii_lowercase();
    vendor == "hp"
        || vendor.starts_with("hewlett-packard")
        || vendor.starts_with("hewlett packard")
        || vendor.starts_with("hewlett_packard")
}

/// Whether a device name leads with the manufacturer, e.g. `HP OfficeJet Pro 8610` or the
/// underscore spelling `HP_LaserJet_3055` an instance name can use. Leading only: a name
/// that merely mentions HP somewhere is not a claim to be one.
fn names_hp_device(name: &str) -> bool {
    let name = name.trim();
    let first_word = name.split([' ', '_']).next().unwrap_or_default();
    is_hp_vendor(first_word) || is_hp_vendor(name)
}

/// Model names to try against `models.dat`, best first.
///
/// More than one because the two service types spell the model differently and neither
/// spelling is reliably the `models.dat` one:
///
/// - `ty` is what HPLIP itself treats as the model (`base/mdns.py:279` puts it straight
///   into `MDL:`), but AirPrint-style records often suffix it with the last bytes of the
///   serial — `HP OfficeJet Pro 8610 [1A2B3C]` — so the bracketed tail is tried off as
///   well as on;
/// - `mdl` on `_scanner._tcp` omits the manufacturer, and `models.dat` sections include
///   it, so `HP ` goes back in front;
/// - the instance name is the last resort, and is what `libhpdiscovery` builds its URI
///   from — it carries no TXT keys at all in the shipped binary.
fn model_candidates(service: &ServiceInfo, instance: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    let mut push = |candidate: String| {
        let candidate = candidate.trim().to_owned();
        if !candidate.is_empty() && !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    };

    for name in [service.get_property_val_str(TYPE_KEY).unwrap_or_default()]
        .into_iter()
        .chain(
            MODEL_KEYS
                .iter()
                .filter_map(|key| service.get_property_val_str(key)),
        )
        .chain([instance])
    {
        let name = name.trim();
        let name = if names_hp_device(name) {
            name.to_owned()
        } else {
            // A bare `mdl=OfficeJet Pro 8610`: the section names in models.dat all carry
            // the manufacturer, so put it back rather than fail the lookup.
            format!("HP {name}")
        };
        push(without_bracketed_tail(&name).to_owned());
        push(name);
    }

    candidates
}

/// `HP OfficeJet Pro 8610 [1A2B3C]` without its `[…]` tail.
fn without_bracketed_tail(name: &str) -> &str {
    let name = name.trim_end();
    match name.strip_suffix(']').and_then(|rest| rest.rfind('[')) {
        Some(open) => name[..open].trim_end(),
        None => name,
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    fn models(entries: &[(&str, i32)]) -> BTreeMap<String, ModelInfo> {
        entries
            .iter()
            .map(|(key, scan_type)| {
                (
                    (*key).to_owned(),
                    ModelInfo {
                        scan_type: *scan_type,
                        scan_src: 3,
                        ..ModelInfo::default()
                    },
                )
            })
            .collect()
    }

    /// A record as `mdns-sd` hands it over, from `service_type`, instance and TXT pairs.
    fn record(service_type: &str, instance: &str, txt: &[(&str, &str)]) -> ServiceInfo {
        let properties: Vec<(String, String)> = txt
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        ServiceInfo::new(
            service_type,
            instance,
            "printer.local.",
            &[IpAddr::from(Ipv4Addr::new(192, 168, 1, 3))][..],
            8080,
            &properties[..],
        )
        .expect("test service info is well formed")
    }

    /// Captured from the development LAN with `avahi-browse -rpt _uscan._tcp`.
    fn brother_uscan() -> ServiceInfo {
        record(
            "_uscan._tcp.local.",
            "Brother MFC-J5335DW",
            &[
                ("duplex", "F"),
                ("is", "adf,platen"),
                ("cs", "binary,grayscale,color"),
                ("pdl", "application/pdf,image/jpeg"),
                ("note", ""),
                ("ty", "Brother MFC-J5335DW"),
                ("rs", "eSCL"),
                (
                    "adminurl",
                    "http://brother-mfcCE27.local./net/net/airprint.html",
                ),
                ("vers", "2.62"),
                ("txtvers", "1"),
            ],
        )
    }

    /// Captured from the same device with `avahi-browse -rpt _scanner._tcp`.
    fn brother_scanner() -> ServiceInfo {
        record(
            "_scanner._tcp.local.",
            "Brother MFC-J5335DW",
            &[
                ("flatbed", "T"),
                ("feeder", "T"),
                ("button", "T"),
                ("mdl", "MFC-J5335DW"),
                ("mfg", "Brother"),
                ("ty", "Brother MFC-J5335DW"),
                ("adminurl", "http://brother-mfcCE27.local./"),
                ("note", ""),
                ("txtvers", "1"),
            ],
        )
    }

    #[test]
    fn an_hp_uscan_record_resolves_to_its_models_dat_model() {
        let service = record(
            "_uscan._tcp.local.",
            "HP OfficeJet Pro 8610 [1A2B3C]",
            &[
                ("txtvers", "1"),
                ("vers", "2.63"),
                ("rs", "eSCL"),
                ("ty", "HP OfficeJet Pro 8610 [1A2B3C]"),
                ("pdl", "application/pdf,image/jpeg"),
                ("is", "platen,adf"),
                ("duplex", "T"),
            ],
        );

        assert_eq!(
            hp_scan_model(&service, &models(&[("hp_officejet_pro_8610", 7)])).as_deref(),
            Some("HP_OfficeJet_Pro_8610"),
        );
    }

    #[test]
    fn a_scanner_record_puts_the_manufacturer_back_in_front_of_mdl() {
        // `_scanner._tcp` splits manufacturer and model, and `ty` here does not lead with
        // the manufacturer either — the models.dat section still does.
        let service = record(
            "_scanner._tcp.local.",
            "Officejet Pro 8610 [1A2B3C]",
            &[
                ("mfg", "Hewlett-Packard"),
                ("mdl", "Officejet Pro 8610"),
                ("ty", "Officejet Pro 8610"),
                ("button", "T"),
            ],
        );

        assert_eq!(
            hp_scan_model(&service, &models(&[("hp_officejet_pro_8610", 7)])).as_deref(),
            Some("HP_Officejet_Pro_8610"),
        );
    }

    #[test]
    fn the_brother_on_this_lan_is_not_claimed() {
        // Both of its records, against a models.dat that does contain a scanning HP: the
        // rejection has to come from the record, not from an empty model list.
        let models = models(&[("hp_officejet_pro_8610", 7)]);

        assert_eq!(hp_scan_model(&brother_uscan(), &models), None);
        assert_eq!(hp_scan_model(&brother_scanner(), &models), None);
    }

    #[test]
    fn a_manufacturer_key_overrides_a_device_name_that_claims_hp() {
        let mut txt: Vec<(&str, &str)> = vec![("mfg", "Brother"), ("mdl", "MFC-J5335DW")];
        txt.push(("ty", "HP OfficeJet Pro 8610"));
        let service = record("_scanner._tcp.local.", "HP OfficeJet Pro 8610", &txt);

        assert_eq!(
            hp_scan_model(&service, &models(&[("hp_officejet_pro_8610", 7)])),
            None,
        );
    }

    #[test]
    fn a_printer_only_hp_is_not_offered_as_a_scanner() {
        let service = record(
            "_uscan._tcp.local.",
            "HP OfficeJet Pro 251dw Printer",
            &[("ty", "HP OfficeJet Pro 251dw Printer")],
        );

        assert_eq!(
            hp_scan_model(&service, &models(&[("hp_officejet_pro_251dw_printer", 0)])),
            None,
        );
    }

    #[test]
    fn a_model_absent_from_models_dat_is_not_claimed() {
        let service = record(
            "_uscan._tcp.local.",
            "HP Something Unreleased",
            &[("ty", "HP Something Unreleased")],
        );

        assert_eq!(
            hp_scan_model(&service, &models(&[("hp_officejet_pro_8610", 7)])),
            None,
        );
        // Same record, no model list at all: an unreadable models.dat rejects rather than
        // guesses.
        assert_eq!(hp_scan_model(&service, &BTreeMap::new()), None);
    }

    #[test]
    fn model_names_are_normalized_the_way_hplip_normalizes_them() {
        assert_eq!(
            normalize_model_name("HP OfficeJet Pro 8610"),
            "HP_OfficeJet_Pro_8610"
        );
        // A single left-to-right pass, exactly as Python's str.replace: three underscores
        // become two, not one.
        assert_eq!(
            normalize_model_name("HP  LaserJet   3055"),
            "HP_LaserJet__3055"
        );
        assert_eq!(normalize_model_name("HP~LaserJet/3055"), "HPLaserJet_3055");
        assert_eq!(
            normalize_model_name("_HP_LaserJet_3055_"),
            "HP_LaserJet_3055"
        );
        // `/` becomes `_` after the collapse, so a slash next to a space keeps both.
        assert_eq!(
            normalize_model_name("HP LaserJet 3055 PCL5/ PS"),
            "HP_LaserJet_3055_PCL5__PS"
        );
        assert_eq!(model_key("HP OfficeJet Pro 8610"), "hp_officejet_pro_8610");
    }

    #[test]
    fn a_bracketed_serial_suffix_is_tried_both_ways() {
        assert_eq!(
            without_bracketed_tail("HP OfficeJet Pro 8610 [1A2B3C]"),
            "HP OfficeJet Pro 8610"
        );
        assert_eq!(
            without_bracketed_tail("HP LaserJet 3055"),
            "HP LaserJet 3055"
        );

        // A model whose real name ends in brackets still resolves, because the untrimmed
        // spelling is a candidate too.
        let service = record(
            "_uscan._tcp.local.",
            "HP ScanJet [Pro]",
            &[("ty", "HP ScanJet [Pro]")],
        );
        assert_eq!(
            hp_scan_model(&service, &models(&[("hp_scanjet_[pro]", 7)])).as_deref(),
            Some("HP_ScanJet_[Pro]"),
        );
    }

    #[test]
    fn the_instance_name_answers_when_there_is_no_ty() {
        let service = record("_uscan._tcp.local.", "HP_LaserJet_3055", &[("rs", "eSCL")]);

        assert_eq!(
            hp_scan_model(&service, &models(&[("hp_laserjet_3055", 4)])).as_deref(),
            Some("HP_LaserJet_3055"),
        );
    }
}
