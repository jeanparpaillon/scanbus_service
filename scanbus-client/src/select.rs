//! A selector, and the object it names ([`scanbus-cli.md`] §5).
//!
//! Nobody types `brother_net_192_2E168_2E1_2E23` twice, so four spellings resolve to one
//! scanner: an object path, an exact `Id`, a unique prefix of an `Id`, a unique substring
//! of a `Name`. The interesting half of that is not the matching — it is what happens when
//! the shortcut is wrong.
//!
//! # Ambiguity is an error, never a pick
//!
//! `unpair MFC` with two Brother scanners on the network, unpairing whichever sorted
//! first, is a data-loss bug wearing a convenience feature's clothes. A stage that matches
//! more than once therefore fails with the candidates — [`SelectError::Ambiguous`] — and
//! does not fall through to the next spelling either, because "this prefix is ambiguous"
//! is a fact the user has to see rather than one to route around.
//!
//! A selector matching nothing gets the same exit code and a different sentence: "you
//! named something that does not exist" is not "the daemon refused", which is why §8 gives
//! it a code of its own instead of folding it into 1.
//!
//! # `Id` is stable, `Name` is not
//!
//! A `Name` comes from the device and moves under a firmware update or a rename on the
//! front panel; an `Id` is contractual ([1.2]). [`Match::ExactId`] — the CLI's `--id` — is
//! what a script pins itself to, and it refuses the other three spellings *even when they
//! are unambiguous today*: a shortcut that resolves now and selects a different scanner
//! after a firmware update is exactly the failure the flag exists to rule out.
//!
//! # One `GetManagedObjects`, and two things that follow from it
//!
//! Resolution reads the object tree once, into [`Objects`], and every selector of one
//! command is matched against that one snapshot: `button set MFC 2` must not ask the
//! daemon twice and let the two answers disagree.
//!
//! The snapshot is a photograph of something transient. An unpaired scanner exists only
//! while a discovery session does (API §1), and a job's object outlives the job by sixty
//! seconds (§4) — so an object can resolve and be gone before the call that follows
//! reaches it. [`Scanner::gone`] and its two siblings turn that into a sentence naming the
//! object, rather than a `zbus` error dump about `UnknownObject`.
//!
//! And resolution only *reads*: nothing here calls `StartDiscovery`. A `show` that probed
//! the network as a side effect would be a surprise, and holding a discovery session is
//! the business of the commands where it is intended ([8.5], [8.6]).
//!
//! [1.2]: https://github.com/jeanparpaillon/scanbus_service/issues/2
//! [8.5]: https://github.com/jeanparpaillon/scanbus_service/issues/32
//! [8.6]: https://github.com/jeanparpaillon/scanbus_service/issues/33
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

use std::fmt;

use scanbus_core::{ScannerId, path};
use zbus::Connection;
use zbus::fdo::ManagedObjects;
use zbus::zvariant::Value as ZValue;

use crate::error::{Error, Result};
use crate::proxy::{BUTTON_INTERFACE, JOB_INTERFACE, SCANNER_INTERFACE};

/// How many of §5's spellings a scanner selector may be.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Match {
    /// Path, exact `Id`, unique `Id` prefix, unique `Name` substring — §5's four, tried
    /// in that order.
    #[default]
    Any,

    /// The argument is an `Id` and nothing else — the CLI's `--id`.
    ExactId,
}

/// The kinds of object a selector can name.
///
/// Carried by the errors rather than baked into their sentences, so that the three
/// resolvers share one report and a caller can branch on *what* was not found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    /// An `org.scanbus.Scanner1`.
    Scanner,
    /// An `org.scanbus.Button1`.
    Button,
    /// An `org.scanbus.Job1`.
    Job,
    /// An `org.scanbus.Profile1`.
    Profile,
}

impl ObjectKind {
    /// The word for one of them.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scanner => "scanner",
            Self::Button => "button",
            Self::Job => "job",
            Self::Profile => "profile",
        }
    }

    /// The word for several of them, for the "known …" heading of a failed match.
    pub const fn plural(self) -> &'static str {
        match self {
            Self::Scanner => "scanners",
            Self::Button => "buttons",
            Self::Job => "jobs",
            Self::Profile => "profiles",
        }
    }

    /// What to do instead, printed under an ambiguity.
    ///
    /// One line, and specific: "be more precise" tells a user nothing they had not worked
    /// out, whereas the spelling that cannot be ambiguous is different for each kind.
    const fn advice(self) -> &'static str {
        match self {
            Self::Scanner => "use the full id — it is stable, a name is not",
            Self::Button => "use the button's index",
            Self::Job => "use the full object path",
            Self::Profile => "use one of the exported profile names",
        }
    }

    /// Why an object of this kind stops existing, appended to [`Error::Vanished`].
    ///
    /// The disappearance is normal for all three, and a message that only says "it is
    /// gone" invites a bug report; each of these names the lifetime the object actually
    /// has.
    pub(crate) const fn hint(self) -> &'static str {
        match self {
            Self::Scanner => " — an unpaired scanner exists only while a discovery session does",
            Self::Button => " — a button goes away with the scanner that owns it",
            Self::Job => " — a job's object is unexported shortly after the job finishes",
            Self::Profile => "",
        }
    }
}

impl fmt::Display for ObjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A selector that named no object, or more than one.
///
/// Both are exit 4 ([`scanbus-cli.md`] §8) and both carry the list a user needs to fix
/// the invocation: the candidates for an ambiguity, everything the daemon exports for a
/// miss.
///
/// [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectError {
    /// Nothing matched.
    NotFound {
        /// What was being looked for.
        kind: ObjectKind,
        /// The selector as the user wrote it.
        selector: String,
        /// Every object of that kind the snapshot held, as candidate labels.
        known: Vec<String>,
    },

    /// More than one matched, at the same stage of §5's ladder.
    Ambiguous {
        /// What was being looked for.
        kind: ObjectKind,
        /// The selector as the user wrote it.
        selector: String,
        /// The objects that matched, as candidate labels.
        candidates: Vec<String>,
    },
}

impl SelectError {
    /// What was being looked for.
    pub const fn kind(&self) -> ObjectKind {
        match self {
            Self::NotFound { kind, .. } | Self::Ambiguous { kind, .. } => *kind,
        }
    }

    /// The selector as the user wrote it.
    pub fn selector(&self) -> &str {
        match self {
            Self::NotFound { selector, .. } | Self::Ambiguous { selector, .. } => selector,
        }
    }

    /// The objects listed under the message: the matches, or everything known.
    pub fn candidates(&self) -> &[String] {
        match self {
            Self::NotFound { known: list, .. }
            | Self::Ambiguous {
                candidates: list, ..
            } => list,
        }
    }
}

impl fmt::Display for SelectError {
    /// A summary line, then one candidate per line, then what to do about it.
    ///
    /// Multi-line on purpose: the CLI prints `scanbus: <what failed>: <this>`, and the
    /// list is the whole value of the message — an ambiguity report without the ambiguous
    /// ids is a riddle.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound {
                kind,
                selector,
                known,
            } => {
                write!(f, "no {kind} matches {selector:?}")?;

                if known.is_empty() {
                    return write!(f, "; the daemon exports no {} right now", kind.plural());
                }

                write!(f, "\nknown {}:", kind.plural())?;
                list(f, known)
            }
            Self::Ambiguous {
                kind,
                selector,
                candidates,
            } => {
                write!(
                    f,
                    "{selector:?} matches {} {}:",
                    candidates.len(),
                    kind.plural()
                )?;
                list(f, candidates)?;
                write!(f, "\n{}", kind.advice())
            }
        }
    }
}

impl std::error::Error for SelectError {}

/// One candidate per line, indented under the summary.
fn list(f: &mut fmt::Formatter<'_>, entries: &[String]) -> fmt::Result {
    for entry in entries {
        write!(f, "\n  {entry}")?;
    }
    Ok(())
}

/// A scanner a selector resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scanner {
    /// `Id` — what the object path is built from, and what a script should select on.
    pub id: ScannerId,
    /// `Name` — the device's label, empty if it reported none.
    pub name: String,
}

impl Scanner {
    /// The object path this scanner is exported at.
    pub fn path(&self) -> String {
        path::scanner(&self.id)
    }

    /// Re-reports a refusal that means "no such object" as this scanner having gone.
    ///
    /// Anything else passes through unchanged — in particular `UnknownMethod`, which is
    /// how a daemon that omits the optional `Scan()` answers (§11.2) and is a statement
    /// about the *daemon*, not about this scanner.
    pub fn gone(&self, error: impl Into<Error>) -> Error {
        gone(ObjectKind::Scanner, self.id.as_str(), error)
    }

    /// How this scanner appears in a candidate list.
    fn label(&self) -> String {
        if self.name.is_empty() {
            self.id.as_str().to_owned()
        } else {
            format!("{} ({})", self.id, self.name)
        }
    }
}

/// A button a selector resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button {
    /// The scanner it belongs to.
    pub scanner: ScannerId,
    /// `Index` — its position in the physical menu, and the path element that names it.
    pub index: u32,
    /// `DeviceLabel` — what the firmware engraved on it, empty when it exposes none.
    pub device_label: String,
}

impl Button {
    /// The object path this button is exported at.
    pub fn path(&self) -> String {
        path::button(&self.scanner, self.index)
    }

    /// Re-reports "no such object" as this button having gone. See [`Scanner::gone`].
    pub fn gone(&self, error: impl Into<Error>) -> Error {
        gone(
            ObjectKind::Button,
            format!("{} of {}", self.index, self.scanner),
            error,
        )
    }

    /// How this button appears in a candidate list.
    fn label(&self) -> String {
        if self.device_label.is_empty() {
            self.index.to_string()
        } else {
            format!("{} ({})", self.index, self.device_label)
        }
    }
}

/// A job a selector resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// The scanner it came from.
    pub scanner: ScannerId,
    /// The job id — the last element of its path, and the short id listings print.
    pub id: u64,
}

impl Job {
    /// The object path this job is exported at.
    pub fn path(&self) -> String {
        path::job(&self.scanner, self.id)
    }

    /// Re-reports "no such object" as this job having gone. See [`Scanner::gone`].
    pub fn gone(&self, error: impl Into<Error>) -> Error {
        gone(
            ObjectKind::Job,
            format!("{} of {}", self.id, self.scanner),
            error,
        )
    }

    /// How this job appears in a candidate list: the path, since two scanners can each
    /// have a job 7 and the short id is then not an answer.
    fn label(&self) -> String {
        self.path()
    }
}

/// The names a bus answers with when the object is not there any more.
///
/// `UnknownInterface` as well as `UnknownObject`, because zbus answers the first when the
/// path still exists as an ancestor of some other object — which is exactly what a scanner
/// that lost its `Scanner1` while keeping a child looks like.
const GONE: [&str; 2] = [
    "org.freedesktop.DBus.Error.UnknownObject",
    "org.freedesktop.DBus.Error.UnknownInterface",
];

/// The shared body of the three `gone` methods.
fn gone(kind: ObjectKind, name: impl Into<String>, error: impl Into<Error>) -> Error {
    let error = error.into();

    match &error {
        Error::Call(refusal) if GONE.contains(&refusal.name()) => Error::Vanished {
            kind,
            name: name.into(),
        },
        _ => error,
    }
}

/// One `GetManagedObjects` reply, reduced to what §5 matches on.
///
/// Every object is kept in a deterministic order — a `ManagedObjects` is a hash map, and
/// a candidate list that reorders between two runs is one a user cannot diff.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Objects {
    scanners: Vec<Scanner>,
    buttons: Vec<Button>,
    jobs: Vec<Job>,
}

impl Objects {
    /// Reads the object tree, once.
    ///
    /// # Errors
    ///
    /// [`Error::Call`] if the daemon refuses `GetManagedObjects`, [`Error::Bus`] if it
    /// never answers.
    pub async fn fetch(connection: &Connection) -> Result<Self> {
        let manager = crate::proxy::object_manager(connection).await?;
        Ok(Self::from_managed(&manager.get_managed_objects().await?))
    }

    /// Reduces a `GetManagedObjects` reply, ignoring everything that is not one of the
    /// three selectable kinds.
    ///
    /// An object whose properties are missing or malformed is *not* dropped: the id of a
    /// scanner, the index of a button and the id of a job are all in the path, which the
    /// daemon cannot have exported wrongly, so the path is the fallback and only `Name`
    /// and `DeviceLabel` — the two that merely make a selector nicer to type — default to
    /// empty. Skipping the object instead would report a scanner that exists as unknown,
    /// which is the one answer resolution must never give.
    pub fn from_managed(managed: &ManagedObjects) -> Self {
        let mut objects = Self::default();

        for (path, interfaces) in managed {
            let path = path.as_str();

            if let Some(properties) = interfaces.get(SCANNER_INTERFACE) {
                if let Some(id) = scanner_id(path, properties) {
                    objects.scanners.push(Scanner {
                        id,
                        name: string(properties, "Name"),
                    });
                }
            } else if let Some(properties) = interfaces.get(BUTTON_INTERFACE) {
                if let Some((scanner, index)) = path::button_index(path) {
                    objects.buttons.push(Button {
                        scanner,
                        index,
                        device_label: string(properties, "DeviceLabel"),
                    });
                }
            } else if interfaces.contains_key(JOB_INTERFACE)
                && let Some((scanner, id)) = path::job_id(path)
            {
                objects.jobs.push(Job { scanner, id });
            }
        }

        objects.scanners.sort_by(|a, b| a.id.cmp(&b.id));
        objects
            .buttons
            .sort_by(|a, b| (&a.scanner, a.index).cmp(&(&b.scanner, b.index)));
        objects
            .jobs
            .sort_by(|a, b| (&a.scanner, a.id).cmp(&(&b.scanner, b.id)));
        objects
    }

    /// Every scanner in the snapshot, by `Id`.
    pub fn scanners(&self) -> &[Scanner] {
        &self.scanners
    }

    /// Every button in the snapshot, by scanner and index.
    pub fn buttons(&self) -> &[Button] {
        &self.buttons
    }

    /// Every job in the snapshot, by scanner and job id.
    pub fn jobs(&self) -> &[Job] {
        &self.jobs
    }

    /// The scanner a selector names, per §5's ladder.
    ///
    /// Path, exact `Id`, unique case-insensitive `Id` prefix, unique case-insensitive
    /// `Name` substring — the first stage that matches anything decides, whether it
    /// decides on one object or refuses because it matched several.
    ///
    /// # Errors
    ///
    /// [`SelectError::NotFound`] when no stage matched, [`SelectError::Ambiguous`] when
    /// one matched more than once.
    pub fn scanner(
        &self,
        selector: &str,
        matching: Match,
    ) -> std::result::Result<&Scanner, SelectError> {
        let kind = ObjectKind::Scanner;
        let missing = || SelectError::NotFound {
            kind,
            selector: selector.to_owned(),
            known: self.scanners.iter().map(Scanner::label).collect(),
        };

        // An empty selector would be a prefix of everything and a substring of every
        // name; it names nothing instead.
        if selector.is_empty() {
            return Err(missing());
        }

        if matching == Match::ExactId {
            return self
                .scanners
                .iter()
                .find(|scanner| scanner.id.as_str() == selector)
                .ok_or_else(missing);
        }

        // A leading slash is unambiguous intent, and a path that names nothing is not
        // then retried as a name substring.
        if selector.starts_with('/') {
            return self
                .scanners
                .iter()
                .find(|scanner| scanner.path() == selector)
                .ok_or_else(missing);
        }

        let lowered = selector.to_lowercase();
        let stages = [
            self.scanners
                .iter()
                .filter(|scanner| scanner.id.as_str() == selector)
                .collect::<Vec<_>>(),
            self.scanners
                .iter()
                .filter(|scanner| scanner.id.as_str().to_lowercase().starts_with(&lowered))
                .collect(),
            self.scanners
                .iter()
                .filter(|scanner| scanner.name.to_lowercase().contains(&lowered))
                .collect(),
        ];

        decide(kind, selector, stages, Scanner::label).unwrap_or_else(|| Err(missing()))
    }

    /// The button of `scanner` a selector names: an `Index`, or a unique
    /// case-insensitive substring of a `DeviceLabel`.
    ///
    /// A selector that parses as a number is an index and is not retried as a label — a
    /// numeric shortcut that silently became a label match on a device whose menu is one
    /// entry shorter would be the same class of bug as an ambiguous unpair.
    ///
    /// # Errors
    ///
    /// As [`Objects::scanner`].
    pub fn button(
        &self,
        scanner: &ScannerId,
        selector: &str,
    ) -> std::result::Result<&Button, SelectError> {
        let kind = ObjectKind::Button;
        let of_scanner = || {
            self.buttons
                .iter()
                .filter(move |button| &button.scanner == scanner)
        };
        let missing = || SelectError::NotFound {
            kind,
            selector: selector.to_owned(),
            known: of_scanner().map(Button::label).collect(),
        };

        if selector.is_empty() {
            return Err(missing());
        }

        if let Ok(index) = selector.parse::<u32>() {
            return of_scanner()
                .find(|button| button.index == index)
                .ok_or_else(missing);
        }

        let lowered = selector.to_lowercase();
        let stages = [
            of_scanner()
                .filter(|button| button.device_label.eq_ignore_ascii_case(selector))
                .collect::<Vec<_>>(),
            of_scanner()
                .filter(|button| {
                    !button.device_label.is_empty()
                        && button.device_label.to_lowercase().contains(&lowered)
                })
                .collect(),
        ];

        decide(kind, selector, stages, Button::label).unwrap_or_else(|| Err(missing()))
    }

    /// The job a selector names: the short id printed by listings, or a full path.
    ///
    /// The short id is the last path element, which is unique per scanner and not across
    /// them — two scanners can each be running their job 7, and that is an ambiguity like
    /// any other rather than a coin toss.
    ///
    /// # Errors
    ///
    /// As [`Objects::scanner`].
    pub fn job(&self, selector: &str) -> std::result::Result<&Job, SelectError> {
        let kind = ObjectKind::Job;
        let missing = || SelectError::NotFound {
            kind,
            selector: selector.to_owned(),
            known: self.jobs.iter().map(Job::label).collect(),
        };

        if selector.is_empty() {
            return Err(missing());
        }

        if selector.starts_with('/') {
            return self
                .jobs
                .iter()
                .find(|job| job.path() == selector)
                .ok_or_else(missing);
        }

        let stages = [self
            .jobs
            .iter()
            .filter(|job| job.id.to_string() == selector)
            .collect::<Vec<_>>()];

        decide(kind, selector, stages, Job::label).unwrap_or_else(|| Err(missing()))
    }
}

/// The first stage that matched anything, or the ambiguity it is.
///
/// `None` means every stage came up empty, which is the caller's `NotFound` — the only
/// outcome that needs to know what *else* exists.
fn decide<'a, T, const N: usize>(
    kind: ObjectKind,
    selector: &str,
    stages: [Vec<&'a T>; N],
    label: impl Fn(&T) -> String,
) -> Option<std::result::Result<&'a T, SelectError>> {
    for matched in stages {
        match matched.as_slice() {
            [] => {}
            [one] => return Some(Ok(one)),
            many => {
                return Some(Err(SelectError::Ambiguous {
                    kind,
                    selector: selector.to_owned(),
                    candidates: many.iter().map(|object| label(object)).collect(),
                }));
            }
        }
    }

    None
}

/// Resolves a scanner selector against a fresh snapshot of the object tree.
///
/// One `GetManagedObjects` and nothing else — no `StartDiscovery`, so a command that
/// resolves a selector does not go looking for scanners as a side effect.
///
/// # Errors
///
/// [`Error::Select`] for a selector that named nothing or too much, and whatever
/// [`Objects::fetch`] can fail with.
pub async fn resolve_scanner(
    connection: &Connection,
    selector: &str,
    matching: Match,
) -> Result<Scanner> {
    Ok(Objects::fetch(connection)
        .await?
        .scanner(selector, matching)?
        .clone())
}

/// Resolves a button selector against a fresh snapshot, within one scanner.
///
/// Prefer [`Objects::button`] on a snapshot already read when the command resolves a
/// scanner too: two `GetManagedObjects` calls can disagree, and §5 costs one.
///
/// # Errors
///
/// As [`resolve_scanner`].
pub async fn resolve_button(
    connection: &Connection,
    scanner: &ScannerId,
    selector: &str,
) -> Result<Button> {
    Ok(Objects::fetch(connection)
        .await?
        .button(scanner, selector)?
        .clone())
}

/// Resolves a job selector against a fresh snapshot.
///
/// # Errors
///
/// As [`resolve_scanner`].
pub async fn resolve_job(connection: &Connection, selector: &str) -> Result<Job> {
    Ok(Objects::fetch(connection).await?.job(selector)?.clone())
}

/// A scanner's `Id`, preferring the property and falling back to the path.
fn scanner_id(path: &str, properties: &crate::convert::Dict) -> Option<ScannerId> {
    properties
        .get("Id")
        .and_then(|value| match &**value {
            ZValue::Str(text) => ScannerId::new(text.as_str()).ok(),
            _ => None,
        })
        .or_else(|| path::scanner_id(path))
}

/// A string property, or `""` — every caller here is reading a label, not a fact.
fn string(properties: &crate::convert::Dict, key: &str) -> String {
    match properties.get(key).map(|value| &**value) {
        Some(ZValue::Str(text)) => text.as_str().to_owned(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use zbus::names::OwnedInterfaceName;
    use zbus::zvariant::{OwnedObjectPath, OwnedValue};

    use super::*;
    use crate::ScanbusError;

    /// One exported object: the path it is at, the interface it carries, and the string
    /// properties of it a selector reads.
    type Fixture<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);

    /// A `GetManagedObjects` reply built from [`Fixture`] triples.
    ///
    /// The fixtures go through the real reply shape rather than constructing [`Objects`]
    /// directly, because reading `Id` and `Name` off the wire is half of what can go
    /// wrong here — a resolution table tested against a hand-built snapshot would not
    /// notice a scanner whose properties never arrived.
    fn managed(objects: &[Fixture<'_>]) -> ManagedObjects {
        let mut reply = ManagedObjects::new();

        for (path, interface, properties) in objects {
            let properties: HashMap<String, OwnedValue> = properties
                .iter()
                .map(|(key, value)| {
                    (
                        (*key).to_owned(),
                        OwnedValue::try_from(ZValue::from(*value)).unwrap(),
                    )
                })
                .collect();

            reply
                .entry(OwnedObjectPath::try_from(*path).unwrap())
                .or_default()
                .insert(
                    OwnedInterfaceName::try_from(*interface).unwrap(),
                    properties,
                );
        }

        reply
    }

    /// Two Brothers on the same subnet — the case every ambiguity assertion needs — plus
    /// an eSCL device whose name shares no substring with them.
    fn two_brothers() -> Objects {
        Objects::from_managed(&managed(&[
            (
                "/org/scanbus/scanner/brother_net_192_2E168_2E1_2E23",
                SCANNER_INTERFACE,
                &[
                    ("Id", "brother_net_192_2E168_2E1_2E23"),
                    ("Name", "MFC-L2710DW"),
                ],
            ),
            (
                "/org/scanbus/scanner/brother_net_192_2E168_2E1_2E24",
                SCANNER_INTERFACE,
                &[
                    ("Id", "brother_net_192_2E168_2E1_2E24"),
                    ("Name", "MFC-J5330DW"),
                ],
            ),
            (
                "/org/scanbus/scanner/escl_avahi_HP_OfficeJet_8010",
                SCANNER_INTERFACE,
                &[
                    ("Id", "escl_avahi_HP_OfficeJet_8010"),
                    ("Name", "HP OfficeJet 8010"),
                ],
            ),
        ]))
    }

    fn id(text: &str) -> ScannerId {
        ScannerId::new(text).unwrap()
    }

    #[test]
    fn the_reply_is_reduced_to_the_three_selectable_kinds_in_a_fixed_order() {
        let objects = Objects::from_managed(&managed(&[
            (
                "/org/scanbus/scanner/b_two",
                SCANNER_INTERFACE,
                &[("Id", "b_two"), ("Name", "Two")],
            ),
            (
                "/org/scanbus/scanner/a_one",
                SCANNER_INTERFACE,
                &[("Id", "a_one"), ("Name", "One")],
            ),
            (
                "/org/scanbus/scanner/a_one/button/2",
                BUTTON_INTERFACE,
                &[("DeviceLabel", "Scan to OCR")],
            ),
            (
                "/org/scanbus/scanner/a_one/button/0",
                BUTTON_INTERFACE,
                &[("DeviceLabel", "Scan to File")],
            ),
            ("/org/scanbus/scanner/a_one/job/7", JOB_INTERFACE, &[]),
            // Neither selectable nor a reason to drop anything else.
            ("/org/scanbus/profile/document", "org.scanbus.Profile1", &[]),
            ("/org/scanbus", "org.scanbus.Manager1", &[]),
        ]));

        assert_eq!(
            objects
                .scanners()
                .iter()
                .map(|scanner| scanner.id.as_str())
                .collect::<Vec<_>>(),
            ["a_one", "b_two"],
            "candidate lists must not reorder between runs"
        );
        assert_eq!(
            objects
                .buttons()
                .iter()
                .map(|button| button.index)
                .collect::<Vec<_>>(),
            [0, 2]
        );
        assert_eq!(
            objects.jobs(),
            [Job {
                scanner: id("a_one"),
                id: 7
            }]
        );
    }

    /// §5's ladder, one rung at a time.
    ///
    /// The two Brothers differ only in their last character, which is the point: a
    /// *unique* prefix of one of them is the whole id, so the prefix rung is exercised on
    /// the eSCL device — and the two that share everything else are what every ambiguity
    /// assertion below stands on.
    #[test]
    fn each_spelling_of_a_scanner_resolves() {
        let objects = two_brothers();
        let brother = "brother_net_192_2E168_2E1_2E23";
        let escl = "escl_avahi_HP_OfficeJet_8010";

        for (selector, expected) in [
            (
                "/org/scanbus/scanner/brother_net_192_2E168_2E1_2E23",
                brother,
            ), // 1: path
            (brother, brother),       // 2: exact id
            ("escl_avahi", escl),     // 3: id prefix
            ("ESCL_AVAHI", escl),     // 3: case-folded
            ("l2710", brother),       // 4: name substring
            ("officejet 8010", escl), // 4: case-folded
        ] {
            assert_eq!(
                objects.scanner(selector, Match::Any).unwrap().id.as_str(),
                expected,
                "{selector}"
            );
        }
    }

    /// Acceptance: a shared id prefix exits 4 and prints both ids; either full id works.
    #[test]
    fn a_shared_id_prefix_is_ambiguous_and_names_the_candidates() {
        let objects = two_brothers();

        let error = objects
            .scanner("brother", Match::Any)
            .expect_err("two scanners share that prefix");

        let SelectError::Ambiguous { candidates, .. } = &error else {
            panic!("a shared prefix is an ambiguity, not {error:?}");
        };
        assert_eq!(candidates.len(), 2);

        // The message a user reads carries both ids and what to do about it.
        let printed = error.to_string();
        assert!(
            printed.contains("brother_net_192_2E168_2E1_2E23"),
            "{printed}"
        );
        assert!(
            printed.contains("brother_net_192_2E168_2E1_2E24"),
            "{printed}"
        );
        assert!(printed.contains("use the full id"), "{printed}");

        // And the full id of either still resolves.
        for full in [
            "brother_net_192_2E168_2E1_2E23",
            "brother_net_192_2E168_2E1_2E24",
        ] {
            assert_eq!(objects.scanner(full, Match::Any).unwrap().id.as_str(), full);
        }
    }

    /// Acceptance: a `Name` substring matching one resolves, matching two exits 4.
    #[test]
    fn a_name_substring_resolves_when_it_is_unique_and_refuses_when_it_is_not() {
        let objects = two_brothers();

        assert_eq!(
            objects
                .scanner("officejet", Match::Any)
                .unwrap()
                .id
                .as_str(),
            "escl_avahi_HP_OfficeJet_8010"
        );

        let error = objects
            .scanner("MFC", Match::Any)
            .expect_err("both Brothers are MFCs");
        assert!(matches!(error, SelectError::Ambiguous { .. }), "{error:?}");
        // The label is what makes a name ambiguity readable: the ids alone would not say
        // why "MFC" matched them.
        assert!(error.to_string().contains("(MFC-L2710DW)"), "{error}");
    }

    /// An ambiguous stage stops there rather than falling through to the next spelling:
    /// "this prefix is ambiguous" is the fact the user needs, not a different match.
    #[test]
    fn an_ambiguous_stage_does_not_fall_through_to_the_next_one() {
        let objects = Objects::from_managed(&managed(&[
            (
                "/org/scanbus/scanner/mfc_a",
                SCANNER_INTERFACE,
                &[("Id", "mfc_a"), ("Name", "First")],
            ),
            (
                "/org/scanbus/scanner/mfc_b",
                SCANNER_INTERFACE,
                &[("Id", "mfc_b"), ("Name", "Second")],
            ),
            (
                "/org/scanbus/scanner/other",
                SCANNER_INTERFACE,
                // A name only this one has, and an id prefix two others share.
                &[("Id", "other"), ("Name", "mfc mono")],
            ),
        ]));

        let error = objects
            .scanner("mfc", Match::Any)
            .expect_err("the id prefix is ambiguous, whatever the names say");
        let SelectError::Ambiguous { candidates, .. } = error else {
            panic!("the prefix stage decides");
        };
        assert_eq!(candidates, ["mfc_a (First)", "mfc_b (Second)"]);
    }

    /// Acceptance: `--id MFC` fails even when `MFC` is an unambiguous name substring.
    #[test]
    fn exact_id_matching_refuses_every_other_spelling() {
        let objects = two_brothers();

        for selector in [
            "MFC-L2710DW",                                         // the name itself
            "l2710",                                               // a unique substring of it
            "escl_avahi",                                          // a unique id prefix
            "/org/scanbus/scanner/brother_net_192_2E168_2E1_2E23", // the path
            "BROTHER_NET_192_2E168_2E1_2E23",                      // the id, miscased
        ] {
            let error = objects
                .scanner(selector, Match::ExactId)
                .expect_err(selector);
            assert!(matches!(error, SelectError::NotFound { .. }), "{selector}");
        }

        assert!(
            objects
                .scanner("brother_net_192_2E168_2E1_2E23", Match::ExactId)
                .is_ok(),
            "the id itself is what --id accepts"
        );
    }

    /// Nothing matched: the message lists what does exist, which is the only way to fix
    /// a typo without a second command.
    #[test]
    fn a_selector_matching_nothing_lists_what_is_known() {
        let error = two_brothers()
            .scanner("epson", Match::Any)
            .expect_err("no Epson here");

        let printed = error.to_string();
        assert!(
            printed.starts_with("no scanner matches \"epson\""),
            "{printed}"
        );
        assert!(printed.contains("known scanners:"), "{printed}");
        assert_eq!(error.candidates().len(), 3, "{printed}");
    }

    /// An empty tree says so, rather than printing an empty list under a heading.
    #[test]
    fn an_empty_tree_says_there_are_none() {
        let error = Objects::default()
            .scanner("anything", Match::Any)
            .expect_err("nothing is exported");

        assert_eq!(
            error.to_string(),
            "no scanner matches \"anything\"; the daemon exports no scanners right now"
        );
    }

    /// An empty selector is a prefix of everything; it names nothing instead.
    #[test]
    fn an_empty_selector_matches_nothing() {
        let objects = two_brothers();
        assert!(objects.scanner("", Match::Any).is_err());
        assert!(
            objects
                .button(&id("brother_net_192_2E168_2E1_2E23"), "")
                .is_err()
        );
        assert!(objects.job("").is_err());
    }

    /// A path that names nothing is not retried as a name substring.
    #[test]
    fn a_path_that_names_nothing_is_not_retried_as_a_substring() {
        let error = two_brothers()
            .scanner("/org/scanbus/scanner/l2710", Match::Any)
            .expect_err("that path is not exported");
        assert!(matches!(error, SelectError::NotFound { .. }), "{error:?}");
    }

    /// A daemon that sends no `Id` still resolves, because the path carries it.
    #[test]
    fn a_scanner_without_properties_is_still_selectable_by_its_path_id() {
        let objects = Objects::from_managed(&managed(&[(
            "/org/scanbus/scanner/mock_usb_1",
            SCANNER_INTERFACE,
            &[],
        )]));

        let scanner = objects.scanner("mock_usb_1", Match::Any).unwrap();
        assert_eq!(scanner.id.as_str(), "mock_usb_1");
        assert_eq!(scanner.name, "", "a missing Name is empty, not a failure");
    }

    fn with_buttons() -> Objects {
        Objects::from_managed(&managed(&[
            (
                "/org/scanbus/scanner/mfc",
                SCANNER_INTERFACE,
                &[("Id", "mfc"), ("Name", "MFC-L2710DW")],
            ),
            (
                "/org/scanbus/scanner/mfc/button/0",
                BUTTON_INTERFACE,
                &[("DeviceLabel", "Scan to File")],
            ),
            (
                "/org/scanbus/scanner/mfc/button/1",
                BUTTON_INTERFACE,
                &[("DeviceLabel", "Scan to Image")],
            ),
            (
                "/org/scanbus/scanner/mfc/button/3",
                BUTTON_INTERFACE,
                &[("DeviceLabel", "Scan to E-mail")],
            ),
            // Another scanner's menu, which must never answer for this one's.
            (
                "/org/scanbus/scanner/other",
                SCANNER_INTERFACE,
                &[("Id", "other"), ("Name", "Elsewhere")],
            ),
            (
                "/org/scanbus/scanner/other/button/0",
                BUTTON_INTERFACE,
                &[("DeviceLabel", "Scan to E-mail")],
            ),
        ]))
    }

    #[test]
    fn a_button_resolves_by_index_and_by_a_unique_label_substring() {
        let objects = with_buttons();
        let mfc = id("mfc");

        assert_eq!(objects.button(&mfc, "3").unwrap().index, 3);
        assert_eq!(objects.button(&mfc, "e-mail").unwrap().index, 3);
        assert_eq!(objects.button(&mfc, "Scan to Image").unwrap().index, 1);

        // "Scan to" is on every key of this menu.
        let error = objects
            .button(&mfc, "Scan to")
            .expect_err("every label starts that way");
        assert!(matches!(error, SelectError::Ambiguous { .. }), "{error:?}");
        assert!(
            error.to_string().contains("use the button's index"),
            "{error}"
        );
    }

    /// The menu is the scanner's own: a label the other device has is not a match here,
    /// and an index this device does not have is not borrowed from there either.
    #[test]
    fn a_button_selector_never_leaves_its_scanner() {
        let objects = with_buttons();

        assert_eq!(objects.button(&id("other"), "e-mail").unwrap().index, 0);

        let error = objects
            .button(&id("mfc"), "2")
            .expect_err("this menu has no key 2");
        let SelectError::NotFound { known, .. } = error else {
            panic!("a missing index is not an ambiguity");
        };
        assert_eq!(
            known,
            [
                "0 (Scan to File)",
                "1 (Scan to Image)",
                "3 (Scan to E-mail)"
            ]
        );
    }

    /// A numeric selector is an index, and is not retried as a label.
    #[test]
    fn a_numeric_button_selector_is_never_a_label() {
        let objects = Objects::from_managed(&managed(&[
            ("/org/scanbus/scanner/x", SCANNER_INTERFACE, &[("Id", "x")]),
            (
                "/org/scanbus/scanner/x/button/0",
                BUTTON_INTERFACE,
                &[("DeviceLabel", "Scan 2 File")],
            ),
        ]));

        assert!(
            objects.button(&id("x"), "2").is_err(),
            "\"2\" is the index that does not exist, not the label that does"
        );
        assert_eq!(objects.button(&id("x"), "0").unwrap().index, 0);
    }

    /// A label that is exactly one entry wins over the substring stage — otherwise a
    /// device with "Scan" and "Scan to File" would have no way to name the first.
    #[test]
    fn an_exact_label_beats_a_longer_one_containing_it() {
        let objects = Objects::from_managed(&managed(&[
            ("/org/scanbus/scanner/x", SCANNER_INTERFACE, &[("Id", "x")]),
            (
                "/org/scanbus/scanner/x/button/0",
                BUTTON_INTERFACE,
                &[("DeviceLabel", "Scan")],
            ),
            (
                "/org/scanbus/scanner/x/button/1",
                BUTTON_INTERFACE,
                &[("DeviceLabel", "Scan to File")],
            ),
        ]));

        assert_eq!(objects.button(&id("x"), "scan").unwrap().index, 0);
    }

    fn with_jobs() -> Objects {
        Objects::from_managed(&managed(&[
            ("/org/scanbus/scanner/a/job/7", JOB_INTERFACE, &[]),
            ("/org/scanbus/scanner/a/job/8", JOB_INTERFACE, &[]),
            ("/org/scanbus/scanner/b/job/7", JOB_INTERFACE, &[]),
        ]))
    }

    #[test]
    fn a_job_resolves_by_short_id_and_by_path() {
        let objects = with_jobs();

        let job = objects.job("8").unwrap();
        assert_eq!((job.scanner.as_str(), job.id), ("a", 8));
        assert_eq!(
            objects
                .job("/org/scanbus/scanner/b/job/7")
                .unwrap()
                .scanner
                .as_str(),
            "b"
        );
    }

    /// A short id is unique per scanner, not across them.
    #[test]
    fn a_short_job_id_two_scanners_share_is_ambiguous() {
        let error = with_jobs()
            .job("7")
            .expect_err("two scanners are running a job 7");

        let SelectError::Ambiguous { candidates, .. } = &error else {
            panic!("not {error:?}");
        };
        assert_eq!(
            candidates,
            &[
                "/org/scanbus/scanner/a/job/7",
                "/org/scanbus/scanner/b/job/7"
            ]
        );
        assert!(
            error.to_string().contains("use the full object path"),
            "{error}"
        );
    }

    /// Acceptance: an object removed between resolution and the call is reported as such,
    /// naming it — not as a raw `UnknownObject`.
    #[test]
    fn an_object_that_disappears_is_named_rather_than_dumped() {
        let scanner = Scanner {
            id: id("brother_net_192_2E168_2E1_2E23"),
            name: "MFC-L2710DW".to_owned(),
        };
        let unknown = ScanbusError::from_reply(
            "org.freedesktop.DBus.Error.UnknownObject",
            "No such object path '/org/scanbus/scanner/brother_net_192_2E168_2E1_2E23'",
        );

        let error = scanner.gone(unknown);
        let printed = error.to_string();

        assert!(matches!(&error, Error::Vanished { kind, .. } if *kind == ObjectKind::Scanner));
        assert!(
            printed.contains("brother_net_192_2E168_2E1_2E23"),
            "{printed}"
        );
        assert!(printed.contains("discovery session"), "{printed}");
        assert!(!printed.contains("UnknownObject"), "{printed}");

        // A button and a job say which lifetime ended, in their own words.
        let button = Button {
            scanner: id("mfc"),
            index: 3,
            device_label: "Scan to E-mail".to_owned(),
        };
        assert!(
            button
                .gone(ScanbusError::from_reply(
                    "org.freedesktop.DBus.Error.UnknownInterface",
                    "",
                ))
                .to_string()
                .contains("button 3 of mfc")
        );

        let job = Job {
            scanner: id("mfc"),
            id: 7,
        };
        let printed = job
            .gone(ScanbusError::from_reply(
                "org.freedesktop.DBus.Error.UnknownObject",
                "",
            ))
            .to_string();
        assert!(
            printed.contains("job 7 of mfc") && printed.contains("unexported"),
            "{printed}"
        );
    }

    /// Every other refusal passes through untouched — `UnknownMethod` above all, which is
    /// a daemon that omits the optional `Scan()` (§11.2) and not a scanner that left.
    #[test]
    fn a_refusal_that_is_not_a_missing_object_is_left_alone() {
        let scanner = Scanner {
            id: id("mfc"),
            name: String::new(),
        };

        for name in [
            "org.freedesktop.DBus.Error.UnknownMethod",
            "org.scanbus.Error.NotReachable",
            "org.freedesktop.DBus.Error.AccessDenied",
        ] {
            let error = scanner.gone(ScanbusError::from_reply(name, "detail"));
            assert!(
                matches!(&error, Error::Call(refusal) if refusal.name() == name),
                "{name} became {error:?}"
            );
        }
    }
}
