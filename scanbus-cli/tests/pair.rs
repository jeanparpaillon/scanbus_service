//! `pair`, `cancel-pairing`, `unpair` against a bus with a mock `Scanner1`
//! ([`scanbus-cli.md`] §7, issue [8.6]).
//!
//! The mock's `Pair()` transitions `PairingState` *synchronously, inside the call*,
//! before it returns — the sharpest version of the race §7 describes: by the time a
//! naive `pair --wait` got around to subscribing, the whole pairing would already be
//! over. Passing against this mock is what proves the subscribe-before-call ordering in
//! [`scanbus_client::watch::ScannerWatch`] actually closes it, rather than merely
//! happening to work when the daemon is slow.
//!
//! [8.6]: https://github.com/jeanparpaillon/scanbus_service/issues/33
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::{PrivateBus, skipped};
use zbus::DBusError;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedValue;

const PAIRED: &str = "brother_net_192_2E168_2E1_2E23";
const UNPAIRED: &str = "escl_avahi_HP_OfficeJet_8010";

/// The one shape §8's named errors take here — a local stand-in for
/// `scanbus-daemon`'s own, since these tests do not depend on that crate
/// ([`common::mod`]'s own module docs explain why).
#[derive(Debug, DBusError)]
#[zbus(prefix = "org")]
enum MockError {
    #[zbus(name = "scanbus.Error.AlreadyPaired")]
    AlreadyPaired(String),
    #[zbus(name = "scanbus.Error.NotPaired")]
    NotPaired(String),
}

/// What `Pair()` does to the mock's state, decided per test.
#[derive(Clone, Copy)]
enum Behaviour {
    /// Flips straight to `done`, inside the call, before it returns.
    SucceedsInstantly,
    /// Flips straight to `failed`, with a fixed message.
    FailsInstantly,
    /// Moves to `pairing` and stops there — for `--no-wait`, which must not block on it.
    NeverFinishes,
}

struct Scanner {
    id: &'static str,
    name: &'static str,
    paired: Mutex<bool>,
    pairing_state: Mutex<String>,
    pairing_error: Mutex<String>,
    behaviour: Behaviour,
    pair_calls: Arc<AtomicUsize>,
    cancel_calls: Arc<AtomicUsize>,
}

#[zbus::interface(name = "org.scanbus.Scanner1")]
impl Scanner {
    #[zbus(property)]
    fn id(&self) -> String {
        self.id.to_owned()
    }

    #[zbus(property)]
    fn name(&self) -> String {
        self.name.to_owned()
    }

    #[zbus(property)]
    fn backend(&self) -> String {
        "mock".to_owned()
    }

    #[zbus(property)]
    fn address(&self) -> String {
        "mock://".to_owned()
    }

    #[zbus(property)]
    fn capabilities(&self) -> HashMap<String, OwnedValue> {
        HashMap::new()
    }

    #[zbus(property)]
    fn supported_profiles(&self) -> Vec<String> {
        vec!["document".to_owned()]
    }

    #[zbus(property)]
    fn paired(&self) -> bool {
        *self.paired.lock().unwrap()
    }

    #[zbus(property)]
    fn connected(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn status(&self) -> String {
        "online".to_owned()
    }

    #[zbus(property)]
    fn default_profile(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn pairing_state(&self) -> String {
        self.pairing_state.lock().unwrap().clone()
    }

    #[zbus(property)]
    fn pairing_error(&self) -> String {
        self.pairing_error.lock().unwrap().clone()
    }

    #[zbus(property)]
    fn pairing_info(&self) -> HashMap<String, OwnedValue> {
        HashMap::new()
    }

    async fn pair(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        _options: HashMap<String, OwnedValue>,
    ) -> Result<(), MockError> {
        self.pair_calls.fetch_add(1, Ordering::SeqCst);

        if *self.paired.lock().unwrap() {
            return Err(MockError::AlreadyPaired(format!(
                "{} is already paired",
                self.id
            )));
        }

        let state = match self.behaviour {
            Behaviour::SucceedsInstantly => {
                *self.paired.lock().unwrap() = true;
                "done"
            }
            Behaviour::FailsInstantly => {
                *self.pairing_error.lock().unwrap() =
                    "brscan4 has no package for this arch".to_owned();
                "failed"
            }
            Behaviour::NeverFinishes => "pairing",
        };
        *self.pairing_state.lock().unwrap() = state.to_owned();

        self.pairing_state_changed(&emitter)
            .await
            .expect("emitting PairingState must not fail on a private bus");
        if matches!(self.behaviour, Behaviour::FailsInstantly) {
            self.pairing_error_changed(&emitter)
                .await
                .expect("emitting PairingError must not fail on a private bus");
        }

        Ok(())
    }

    async fn cancel_pairing(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), MockError> {
        self.cancel_calls.fetch_add(1, Ordering::SeqCst);
        *self.pairing_state.lock().unwrap() = "none".to_owned();
        self.pairing_state_changed(&emitter).await.ok();
        Ok(())
    }

    async fn unpair(&self) -> Result<(), MockError> {
        if !*self.paired.lock().unwrap() {
            return Err(MockError::NotPaired(format!("{} is not paired", self.id)));
        }
        *self.paired.lock().unwrap() = false;
        Ok(())
    }
}

/// A `Manager1` that does nothing beyond answering: `pair` holds a discovery session
/// for the duration of pairing an unpaired scanner (scanbus-cli.md §7), so one has to
/// exist on this bus even though nothing here actually discovers.
struct Manager;

#[zbus::interface(name = "org.scanbus.Manager1")]
impl Manager {
    fn start_discovery(&self, _filters: HashMap<String, OwnedValue>) {}

    fn stop_discovery(&self) {}

    fn get_profile_types(&self) -> Vec<String> {
        vec!["document".to_owned()]
    }
}

/// Owns `org.scanbus` on `address`: one scanner, behaving as `behaviour` says, paired or
/// not as `paired` says.
async fn serve(
    address: &str,
    id: &'static str,
    paired: bool,
    behaviour: Behaviour,
) -> (zbus::Connection, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let pair_calls = Arc::new(AtomicUsize::new(0));
    let cancel_calls = Arc::new(AtomicUsize::new(0));

    let scanner = Scanner {
        id,
        name: "Test Scanner",
        paired: Mutex::new(paired),
        pairing_state: Mutex::new("none".to_owned()),
        pairing_error: Mutex::new(String::new()),
        behaviour,
        pair_calls: Arc::clone(&pair_calls),
        cancel_calls: Arc::clone(&cancel_calls),
    };

    let connection = zbus::connection::Builder::address(address)
        .expect("the private bus address must parse")
        .name("org.scanbus")
        .expect("org.scanbus is a well-known name")
        .serve_at("/org/scanbus", Manager)
        .expect("/org/scanbus is a valid path")
        .serve_at("/org/scanbus", zbus::fdo::ObjectManager)
        .expect("/org/scanbus is a valid path")
        .serve_at(format!("/org/scanbus/scanner/{id}"), scanner)
        .expect("cannot export the scanner")
        .build()
        .await
        .expect("cannot own org.scanbus on the private bus");

    (connection, pair_calls, cancel_calls)
}

/// A bus with one scanner, or `None` when `dbus-daemon` is missing.
async fn fixture(
    id: &'static str,
    paired: bool,
    behaviour: Behaviour,
) -> Option<(
    PrivateBus,
    zbus::Connection,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
)> {
    let bus = PrivateBus::start()?;
    let (daemon, pair_calls, cancel_calls) = serve(bus.address(), id, paired, behaviour).await;
    Some((bus, daemon, pair_calls, cancel_calls))
}

/// Acceptance: a `Pair()` that finishes before the CLI could possibly have looked yet
/// still reports `done` and exits 0 — the race described in scanbus-cli.md §7 is closed.
#[tokio::test(flavor = "multi_thread")]
async fn a_pairing_that_finishes_instantly_still_reports_done() {
    let Some((bus, _daemon, ..)) = fixture(UNPAIRED, false, Behaviour::SucceedsInstantly).await
    else {
        return skipped("a_pairing_that_finishes_instantly_still_reports_done");
    };

    let run = bus.scanbus(&["--json", "pair", UNPAIRED, "--id"]);
    run.assert_code(0);
    let document = run.json();
    assert_eq!(document["PairingState"], "done");
    assert_eq!(document["Paired"], true);
}

/// A pairing that ends in `failed` is exit 9, with `PairingError` on stderr.
#[tokio::test(flavor = "multi_thread")]
async fn a_pairing_that_fails_is_exit_nine() {
    let Some((bus, _daemon, ..)) = fixture(UNPAIRED, false, Behaviour::FailsInstantly).await else {
        return skipped("a_pairing_that_fails_is_exit_nine");
    };

    let run = bus.scanbus(&["pair", UNPAIRED, "--id"]);
    run.assert_code(9);
    assert!(
        run.stderr.contains("brscan4 has no package for this arch"),
        "{}",
        run.stderr
    );
}

/// `Pair()` on an already-paired scanner is exit 6, and the message points at `unpair`.
#[tokio::test(flavor = "multi_thread")]
async fn already_paired_exits_six_and_points_at_unpair() {
    let Some((bus, _daemon, ..)) = fixture(PAIRED, true, Behaviour::SucceedsInstantly).await else {
        return skipped("already_paired_exits_six_and_points_at_unpair");
    };

    let run = bus.scanbus(&["pair", PAIRED, "--id"]);
    run.assert_code(6);
    assert!(run.stderr.contains("scanbus unpair"), "{}", run.stderr);
}

/// `--no-wait` returns as soon as `Pair()` does, without waiting for a state that never
/// arrives.
#[tokio::test(flavor = "multi_thread")]
async fn no_wait_returns_without_reaching_a_terminal_state() {
    let Some((bus, _daemon, ..)) = fixture(UNPAIRED, false, Behaviour::NeverFinishes).await else {
        return skipped("no_wait_returns_without_reaching_a_terminal_state");
    };

    let run = bus.scanbus(&["--json", "pair", UNPAIRED, "--id", "--no-wait"]);
    run.assert_code(0);
}

/// `cancel-pairing` calls `CancelPairing()`.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_pairing_calls_the_method() {
    let Some((bus, _daemon, _pair_calls, cancel_calls)) =
        fixture(UNPAIRED, false, Behaviour::NeverFinishes).await
    else {
        return skipped("cancel_pairing_calls_the_method");
    };

    bus.scanbus(&["cancel-pairing", UNPAIRED, "--id"])
        .assert_code(0);
    assert_eq!(cancel_calls.load(Ordering::SeqCst), 1);
}

/// `unpair --yes` calls `Unpair()` without prompting.
#[tokio::test(flavor = "multi_thread")]
async fn unpair_with_yes_calls_the_method() {
    let Some((bus, _daemon, ..)) = fixture(PAIRED, true, Behaviour::SucceedsInstantly).await else {
        return skipped("unpair_with_yes_calls_the_method");
    };

    bus.scanbus(&["unpair", PAIRED, "--id", "--yes"])
        .assert_code(0);
}

/// `unpair` on a scanner that is not paired is exit 7.
#[tokio::test(flavor = "multi_thread")]
async fn unpair_not_paired_exits_seven() {
    let Some((bus, _daemon, ..)) = fixture(UNPAIRED, false, Behaviour::SucceedsInstantly).await
    else {
        return skipped("unpair_not_paired_exits_seven");
    };

    bus.scanbus(&["unpair", UNPAIRED, "--id", "--yes"])
        .assert_code(7);
}

/// Without `--yes` and a non-terminal stdin (what every test subprocess has), `unpair`
/// refuses rather than hanging on a prompt.
#[tokio::test(flavor = "multi_thread")]
async fn unpair_without_yes_on_a_non_tty_stdin_fails_rather_than_hangs() {
    let Some((bus, _daemon, ..)) = fixture(PAIRED, true, Behaviour::SucceedsInstantly).await else {
        return skipped("unpair_without_yes_on_a_non_tty_stdin_fails_rather_than_hangs");
    };

    let run = bus.scanbus(&["unpair", PAIRED, "--id"]);
    assert_ne!(run.code, 0, "must refuse rather than silently unpair");
}
