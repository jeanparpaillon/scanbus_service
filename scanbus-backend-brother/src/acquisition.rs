//! Turning "button 2 was pressed" into a `scanimage` command line.
//!
//! Acquisition goes over **eSCL**, through the packaged `scanbus-scanimage` helper and
//! [`fetch_pages_via_scanimage`](scanbus_backend_common::fetch_pages_via_scanimage) —
//! the same path HP takes (6.3), so the ADF batching and the end-of-feed rule live in
//! one crate instead of being written twice
//! ([`brother-skeyless-backend.md`] §2.1).
//!
//! # Why this module is pure
//!
//! Everything here is a function of a sighting, a button assignment and a profile — no
//! socket, no process, no clock. That is what lets the option mapping be tested without a
//! printer, which matters because the option *vocabulary* is the part that can be wrong:
//! `sane-airscan` on the development machine's MFC-J5335DW offers exactly
//!
//! ```text
//! --resolution 100|200|300|600dpi [300]
//! --mode Color|Gray [Color]
//! --source Flatbed|ADF [Flatbed]
//! ```
//!
//! and a name outside that set is an `Invalid argument` at `sane_start`, i.e. a failed
//! scan after the user has already walked to the printer.
//!
//! [`brother-skeyless-backend.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/brother-skeyless-backend.md

use std::collections::BTreeMap;

use scanbus_core::{ColorMode, ProfileKind, Source, Value};

/// The resolution asked for when nothing says otherwise. Also `sane-airscan`'s own
/// default, so the common case adds no option at all.
pub const DEFAULT_RESOLUTION_DPI: u32 = 300;

/// What an eSCL sighting says this device can be asked for.
///
/// Deliberately the *sighting's* claim and not a per-model table: the table this
/// replaced ([5.1](https://github.com/jeanparpaillon/scanbus_service/issues/18)) mapped
/// five model names to a resolution list and was wrong for every model not in it,
/// silently — a scanner it had never heard of was published with no resolutions at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EsclDevice {
    /// What `scanimage --device-name` is handed.
    pub device_uri: String,
    pub resolutions: Vec<u32>,
    pub color_modes: Vec<ColorMode>,
    pub sources: Vec<Source>,
}

/// What the daemon assigned to one panel key, as `set_button_mapping` recorded it.
///
/// Not `Eq`: [`Value`] carries a `f64` variant, so the option map only has partial
/// equality. Nothing here compares mappings — the derive exists for tests.
#[derive(Debug, Clone, PartialEq)]
pub struct ButtonMapping {
    pub profile: ProfileKind,
    /// `Button1.ProfileOptions` merged over `Profile1.Options`, as the daemon resolved
    /// them (API §5).
    pub options: BTreeMap<String, Value>,
}

/// The `scanimage` invocation one press turns into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acquisition {
    /// Stamped on every [`RawPage`](scanbus_core::RawPage) the run produces, so the
    /// profile pipeline scales the page from what was actually asked for.
    pub resolution_dpi: u32,
    /// Appended to the fixed `--device-name`/`--format`/`--batch` arguments.
    pub args: Vec<String>,
}

/// The eSCL option names for the two enums the API speaks in.
///
/// One place, because a spelling that drifts from `scanimage`'s vocabulary fails at
/// `sane_start` rather than at compile time.
const fn source_name(source: Source) -> &'static str {
    match source {
        Source::Flatbed => "Flatbed",
        Source::Adf => "ADF",
    }
}

const fn mode_name(mode: ColorMode) -> &'static str {
    match mode {
        ColorMode::Color => "Color",
        ColorMode::Gray => "Gray",
        // SANE's own name for bitonal. eSCL devices that offer it advertise it; the
        // MFC-J5335DW does not, and this is never emitted for one that does not.
        ColorMode::Bw => "Lineart",
    }
}

/// The command line for one press.
///
/// `mapping` is `None` for a key the daemon has assigned nothing to. That is not an
/// error here — the press already happened and the paper is already in the feeder — so
/// the device's own defaults are used and the daemon decides what to do with the pages.
///
/// # The option keys
///
/// `source`, `resolution` and `mode` are read out of the button's resolved profile
/// options. They are **not** in `Profile1.OptionsSchema` today — §6 declares `format`,
/// `quality`, `multi_page` and `output_folder`, so the daemon rejects a write of any
/// other key — which means the fallbacks below are what every scan currently uses. They
/// are honoured anyway rather than ignored, because the acquisition side is where they
/// have to land the day the schema grows them, and a mapping written later against a
/// backend that silently dropped them would be found by a user rather than by a test.
///
/// Everything not named falls back to what the *device* defaults to, with one exception:
/// the paper path, which the profile decides. A `document` profile is the multi-page one
/// (§6: its result is a PDF assembled from an ADF run), so it asks for the feeder when
/// the device has one; everything else is a single sheet on the glass. Nothing else is
/// inferred from the profile — a colour mode guessed from "this is a document" is the
/// kind of cleverness that produces a grey photocopy of a colour original.
pub fn scanimage_args(mapping: Option<&ButtonMapping>, device: &EsclDevice) -> Acquisition {
    let options = mapping.map(|mapping| &mapping.options);
    let profile = mapping.map(|mapping| mapping.profile);

    let source = requested_source(options).unwrap_or_else(|| default_source(profile, device));
    let source = offered(source, &device.sources, Source::Flatbed);

    let mode = requested_mode(options).map(|mode| {
        let fallback = device.color_modes.first().copied().unwrap_or(mode);
        offered(mode, &device.color_modes, fallback)
    });

    let resolution = requested_resolution(options).unwrap_or(DEFAULT_RESOLUTION_DPI);
    let resolution = nearest_resolution(resolution, &device.resolutions);

    let mut args = vec![
        format!("--source={}", source_name(source)),
        format!("--resolution={resolution}"),
    ];
    if let Some(mode) = mode {
        args.push(format!("--mode={}", mode_name(mode)));
    }
    // `--batch` with no count asks for pages until the source refuses. That is what an
    // ADF run wants and what a flatbed run must not do: the glass never says "no more
    // documents", so an uncounted batch is a device the user has to walk back to.
    if source == Source::Flatbed {
        args.push("--batch-count=1".to_owned());
    }

    Acquisition {
        resolution_dpi: resolution,
        args,
    }
}

/// The paper path a profile implies when nothing was asked for explicitly.
fn default_source(profile: Option<ProfileKind>, device: &EsclDevice) -> Source {
    match profile {
        Some(ProfileKind::Document) if device.sources.contains(&Source::Adf) => Source::Adf,
        _ => Source::Flatbed,
    }
}

/// Keeps a choice inside what the device advertised.
///
/// An empty list is "the sighting said nothing", not "the device offers nothing": a bare
/// `airscan:` line carries no capability tokens at all, and refusing to scan because of
/// that would make every such device unusable. The choice stands in that case, and
/// `scanimage` is left to reject it if it really is unsupported.
fn offered<T: Copy + PartialEq>(wanted: T, advertised: &[T], fallback: T) -> T {
    if advertised.is_empty() || advertised.contains(&wanted) {
        wanted
    } else {
        fallback
    }
}

/// The advertised resolution closest to what was asked for.
///
/// Snapping rather than refusing: eSCL devices publish a discrete list, and a profile
/// asking for 400 on a 100/200/300/600 device wants the nearest thing, not an error at
/// the end of a walk to the printer. Ties go to the higher one — dropping detail is the
/// less recoverable mistake.
fn nearest_resolution(wanted: u32, advertised: &[u32]) -> u32 {
    advertised
        .iter()
        .copied()
        .min_by_key(|candidate| (candidate.abs_diff(wanted), u32::MAX - candidate))
        .unwrap_or(wanted)
}

fn requested_source(options: Option<&BTreeMap<String, Value>>) -> Option<Source> {
    match option_str(options, "source")?.as_str() {
        "flatbed" | "platen" | "glass" => Some(Source::Flatbed),
        "adf" | "feeder" => Some(Source::Adf),
        _ => None,
    }
}

fn requested_mode(options: Option<&BTreeMap<String, Value>>) -> Option<ColorMode> {
    match option_str(options, "mode")?.as_str() {
        "color" | "colour" | "rgb" => Some(ColorMode::Color),
        "gray" | "grey" | "grayscale" => Some(ColorMode::Gray),
        "bw" | "lineart" | "monochrome" => Some(ColorMode::Bw),
        _ => None,
    }
}

fn requested_resolution(options: Option<&BTreeMap<String, Value>>) -> Option<u32> {
    match options?.get("resolution")? {
        Value::U64(dpi) => u32::try_from(*dpi).ok(),
        Value::I64(dpi) => u32::try_from(*dpi).ok(),
        Value::Str(dpi) => dpi.parse().ok(),
        _ => None,
    }
}

/// A string option, lowercased so the spellings above are the whole vocabulary.
fn option_str(options: Option<&BTreeMap<String, Value>>, key: &str) -> Option<String> {
    match options?.get(key)? {
        Value::Str(text) => Some(text.trim().to_ascii_lowercase()),
        _ => None,
    }
}

/// What a `scanimage -L` description says the device can do, when it says anything.
///
/// The `escl:` backend prints its sources into the description —
/// `is a Brother MFC-J5335DW adf,platen scanner` — and `sane-airscan` prints none. So
/// this returns `None` rather than an empty list for the second case: "the sighting is
/// silent" and "the device has no paper path" have to stay distinguishable, or a silent
/// sighting would publish a scanner with no sources and no way to scan.
pub fn sources_from_description(description: &str) -> Option<Vec<Source>> {
    let lower = description.to_ascii_lowercase();
    let mut sources = Vec::new();
    if lower.contains("platen") || lower.contains("flatbed") {
        sources.push(Source::Flatbed);
    }
    if lower.contains("adf") || lower.contains("feeder") {
        sources.push(Source::Adf);
    }
    (!sources.is_empty()).then_some(sources)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mfc_j5335dw() -> EsclDevice {
        EsclDevice {
            device_uri: "airscan:e0:Brother MFC-J5335DW".to_owned(),
            resolutions: vec![100, 200, 300, 600],
            color_modes: vec![ColorMode::Color, ColorMode::Gray],
            sources: vec![Source::Flatbed, Source::Adf],
        }
    }

    fn mapping(profile: ProfileKind, options: &[(&str, Value)]) -> ButtonMapping {
        ButtonMapping {
            profile,
            options: options
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect(),
        }
    }

    /// The two profiles that have a processor, and the one thing the profile decides.
    #[test]
    fn a_document_takes_the_feeder_and_an_image_takes_the_glass() {
        let document = scanimage_args(Some(&mapping(ProfileKind::Document, &[])), &mfc_j5335dw());
        assert_eq!(
            document.args,
            vec!["--source=ADF".to_owned(), "--resolution=300".to_owned()]
        );

        let image = scanimage_args(Some(&mapping(ProfileKind::Image, &[])), &mfc_j5335dw());
        assert_eq!(
            image.args,
            vec![
                "--source=Flatbed".to_owned(),
                "--resolution=300".to_owned(),
                // The glass never reports "no more documents"; an uncounted batch there
                // is a scanner that keeps scanning.
                "--batch-count=1".to_owned(),
            ]
        );
    }

    /// A flatbed-only device asked for a document gets the glass, not a failed scan.
    #[test]
    fn a_document_on_a_device_with_no_feeder_falls_back_to_the_glass() {
        let flatbed_only = EsclDevice {
            sources: vec![Source::Flatbed],
            ..mfc_j5335dw()
        };

        let acquisition = scanimage_args(
            Some(&mapping(ProfileKind::Document, &[])),
            &flatbed_only,
        );

        assert!(acquisition.args.contains(&"--source=Flatbed".to_owned()));
        assert!(acquisition.args.contains(&"--batch-count=1".to_owned()));
    }

    #[test]
    fn the_options_override_every_fallback() {
        let acquisition = scanimage_args(
            Some(&mapping(
                ProfileKind::Document,
                &[
                    ("source", Value::Str("flatbed".to_owned())),
                    ("mode", Value::Str("Gray".to_owned())),
                    ("resolution", Value::U64(600)),
                ],
            )),
            &mfc_j5335dw(),
        );

        assert_eq!(acquisition.resolution_dpi, 600);
        assert_eq!(
            acquisition.args,
            vec![
                "--source=Flatbed".to_owned(),
                "--resolution=600".to_owned(),
                "--mode=Gray".to_owned(),
                "--batch-count=1".to_owned(),
            ]
        );
    }

    /// The device's list is the authority, and the walk to the printer is what makes
    /// snapping better than refusing.
    #[test]
    fn an_unadvertised_choice_is_snapped_rather_than_sent() {
        let acquisition = scanimage_args(
            Some(&mapping(
                ProfileKind::Image,
                &[
                    ("resolution", Value::U64(450)),
                    ("mode", Value::Str("bw".to_owned())),
                ],
            )),
            &mfc_j5335dw(),
        );

        // 450 is equidistant from 300 and 600, and detail is the thing not to drop.
        assert_eq!(acquisition.resolution_dpi, 600);
        // Bitonal is not on this device's list, so the first offered mode is asked for.
        assert!(acquisition.args.contains(&"--mode=Color".to_owned()));
    }

    /// A sighting that carries no capability tokens must not become a device that can be
    /// asked for nothing.
    #[test]
    fn a_silent_sighting_leaves_the_choice_to_scanimage() {
        let silent = EsclDevice {
            resolutions: Vec::new(),
            color_modes: Vec::new(),
            sources: Vec::new(),
            ..mfc_j5335dw()
        };

        let acquisition = scanimage_args(Some(&mapping(ProfileKind::Document, &[])), &silent);

        // No ADF was advertised, so the profile's feeder is not assumed…
        assert!(acquisition.args.contains(&"--source=Flatbed".to_owned()));
        // …and the resolution asked for is the one nothing contradicted.
        assert_eq!(acquisition.resolution_dpi, DEFAULT_RESOLUTION_DPI);
    }

    /// The paper is already in the feeder when this is discovered, so it is not an error.
    #[test]
    fn an_unassigned_key_still_produces_a_command_line() {
        let acquisition = scanimage_args(None, &mfc_j5335dw());

        assert_eq!(acquisition.resolution_dpi, DEFAULT_RESOLUTION_DPI);
        assert!(acquisition.args.contains(&"--source=Flatbed".to_owned()));
    }

    #[test]
    fn capability_tokens_are_read_off_the_escl_description_when_there_are_any() {
        assert_eq!(
            sources_from_description("Brother MFC-J5335DW adf,platen scanner"),
            Some(vec![Source::Flatbed, Source::Adf])
        );
        assert_eq!(
            sources_from_description("Brother DCP-J1050DW platen scanner"),
            Some(vec![Source::Flatbed])
        );
        // `sane-airscan` prints no tokens at all — that is silence, not "no sources".
        assert_eq!(
            sources_from_description("eSCL Brother MFC-J5335DW ip=192.168.1.3"),
            None
        );
    }
}
