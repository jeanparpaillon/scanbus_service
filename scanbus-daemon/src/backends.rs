//! [`Backends`]: which backends this daemon has, in the order that decides ties.
//!
//! Two things need an ordered list rather than a set. The `filters` argument of
//! `StartDiscovery` (§2) names backends by id, so ids have to be unique and resolvable;
//! and the same physical device is routinely found twice — `sane-airscan` makes every
//! eSCL scanner visible to SANE *and* to a direct eSCL probe — so something has to say
//! which sighting becomes the object. §9 says to deduplicate by physical address and
//! keep `Backend` on the object; it does not say who wins, and this is where that is
//! decided.
//!
//! # Precedence is registration order, most specific first
//!
//! The list is ordered by how much a backend can do for a device it claims, not by how
//! likely it is to find one:
//!
//! 1. **Vendor backends** (`brother-skey`, `hplip`). They are the only ones that can
//!    deliver a *button press* — the entire point of this daemon (README) — because
//!    walk-up events come from `brscan-skey` and `hpssd`, not from SANE.
//! 2. **Generic network backends** (a direct eSCL/Avahi probe), which can scan and
//!    report capabilities but have no event channel.
//! 3. **`sane`** last. It sees the most devices and knows the least about them: through
//!    `sane-airscan` it reports an eSCL device with none of the eSCL metadata, and it
//!    has no notion of the physical menu at all.
//!
//! Picking the vendor backend for a Brother MFC is what makes `Button1` objects exist
//! for it (§5); picking SANE would leave the same device visible, scannable, and unable
//! to do the one thing the user bought it for. The cost is that `Backend` on a
//! deduplicated object names something other than what a `scanimage -L` shows, which is
//! why the property exists (§9).
//!
//! The rule is a total order fixed at startup rather than a per-device negotiation
//! because it has to be explainable: "the daemon lists its backends in this order, and
//! the first one to claim an address owns it" is a sentence a user can act on when
//! `Backend` says something they did not expect.

use std::sync::Arc;

use scanbus_core::ScannerBackend;
use thiserror::Error;
use tracing::warn;

/// A backend, and where it sits in the precedence order.
///
/// `rank` is the index in [`Backends`], so a smaller number wins. It travels with the
/// sighting into [`crate::scanners`], which is where the comparison happens.
#[derive(Clone)]
pub struct RankedBackend {
    /// Position in the list; smaller is more specific, and wins a tie.
    pub rank: usize,
    /// The backend itself.
    pub backend: Arc<dyn ScannerBackend>,
}

/// A `filters` entry naming a backend this daemon does not have.
///
/// A client bug rather than an environment fact — the daemon's backend list is
/// discoverable and static — so it becomes `org.freedesktop.DBus.Error.InvalidArgs`
/// rather than being silently ignored, which would have `StartDiscovery` succeed while
/// probing something the client did not ask for.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("no backend named {name:?}; this daemon has [{}]", known.join(", "))]
pub struct UnknownBackend {
    /// The name the client asked for.
    pub name: String,
    /// The ids this daemon does have, in precedence order.
    pub known: Vec<&'static str>,
}

/// Every backend compiled into this daemon, in precedence order.
///
/// Built once at startup and shared; cheap to clone out of, since the backends
/// themselves are behind [`Arc`].
#[derive(Clone, Default)]
pub struct Backends {
    entries: Vec<Arc<dyn ScannerBackend>>,
}

impl Backends {
    /// Builds the list, most specific backend first.
    ///
    /// A duplicate id is dropped with a warning rather than accepted: two backends
    /// answering to one name would make a `filters` entry ambiguous, and the daemon is
    /// still useful with the first of them.
    pub fn new(backends: impl IntoIterator<Item = Arc<dyn ScannerBackend>>) -> Self {
        let mut entries: Vec<Arc<dyn ScannerBackend>> = Vec::new();

        for backend in backends {
            if entries.iter().any(|other| other.id() == backend.id()) {
                warn!(
                    backend = backend.id(),
                    "a backend with this id is already registered; ignoring the second"
                );
                continue;
            }
            entries.push(backend);
        }

        Self { entries }
    }

    /// The ids, in precedence order — what a `filters` entry may name.
    pub fn ids(&self) -> Vec<&'static str> {
        self.entries.iter().map(|backend| backend.id()).collect()
    }

    /// Whether no backend is compiled in.
    ///
    /// True on a default build: both vendor backends are behind cargo features, so
    /// `StartDiscovery` succeeds and finds nothing. Worth a log line at startup, since
    /// "discovery returns no scanner" otherwise looks like a probe failure.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many backends are registered.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The backends a discovery session should probe, ranked.
    ///
    /// `None` selects all of them. A subset keeps the ranks of the full list, so
    /// filtering changes *which* backends run, never which one wins a tie — a client
    /// that excludes `sane` must not thereby promote it.
    ///
    /// # Errors
    ///
    /// [`UnknownBackend`] for the first name that matches nothing. Reported rather than
    /// skipped, per §2's `filters` being a client-supplied selection.
    pub fn select(&self, names: Option<&[String]>) -> Result<Vec<RankedBackend>, UnknownBackend> {
        let Some(names) = names else {
            return Ok(self.ranked().collect());
        };

        for name in names {
            if !self.entries.iter().any(|backend| backend.id() == name) {
                return Err(UnknownBackend {
                    name: name.clone(),
                    known: self.ids(),
                });
            }
        }

        Ok(self
            .ranked()
            .filter(|entry| names.iter().any(|name| name == entry.backend.id()))
            .collect())
    }

    fn ranked(&self) -> impl Iterator<Item = RankedBackend> + '_ {
        self.entries
            .iter()
            .enumerate()
            .map(|(rank, backend)| RankedBackend {
                rank,
                backend: Arc::clone(backend),
            })
    }
}

#[cfg(test)]
mod tests {
    use scanbus_core::backend::mock::MockBackend;

    use super::*;

    fn backends() -> Backends {
        Backends::new([
            Arc::new(MockBackend::new().with_id("brother-skey")) as Arc<dyn ScannerBackend>,
            Arc::new(MockBackend::new().with_id("escl")),
            Arc::new(MockBackend::new().with_id("sane")),
        ])
    }

    #[test]
    fn precedence_is_registration_order() {
        assert_eq!(backends().ids(), ["brother-skey", "escl", "sane"]);
        assert_eq!(
            backends()
                .select(None)
                .unwrap()
                .into_iter()
                .map(|entry| (entry.rank, entry.backend.id()))
                .collect::<Vec<_>>(),
            [(0, "brother-skey"), (1, "escl"), (2, "sane")]
        );
    }

    /// Excluding a backend must not change what the remaining ones outrank.
    #[test]
    fn a_filtered_subset_keeps_the_full_list_s_ranks() {
        let selected = backends()
            .select(Some(&["sane".to_owned(), "escl".to_owned()]))
            .unwrap();

        assert_eq!(
            selected
                .into_iter()
                .map(|entry| (entry.rank, entry.backend.id()))
                .collect::<Vec<_>>(),
            [(1, "escl"), (2, "sane")]
        );
    }

    #[test]
    fn an_unknown_name_is_refused_with_the_list_of_real_ones() {
        // `err()` rather than `unwrap_err()`: a backend is not `Debug`.
        let error = backends()
            .select(Some(&["avahi".to_owned()]))
            .err()
            .expect("an unknown backend must not be ignored");

        assert_eq!(error.name, "avahi");
        assert_eq!(error.known, ["brother-skey", "escl", "sane"]);
        assert!(
            error.to_string().contains("brother-skey, escl, sane"),
            "the message should name what is available: {error}"
        );
    }

    #[test]
    fn an_empty_filter_selects_nothing_and_is_not_an_error() {
        assert!(backends().select(Some(&[])).unwrap().is_empty());
    }

    #[test]
    fn a_duplicate_id_is_dropped_rather_than_making_a_filter_ambiguous() {
        let backends = Backends::new([
            Arc::new(MockBackend::with_scanners([]).with_id("sane")) as Arc<dyn ScannerBackend>,
            Arc::new(MockBackend::new().with_id("sane")),
        ]);

        assert_eq!(backends.len(), 1);
        assert_eq!(backends.ids(), ["sane"]);
    }

    #[test]
    fn a_default_build_has_no_backend_at_all() {
        assert!(Backends::default().is_empty());
        assert!(Backends::default().select(None).unwrap().is_empty());
    }
}
