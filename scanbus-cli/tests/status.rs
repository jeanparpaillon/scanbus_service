//! `scanbus status` against a bus where the daemon is absent, activatable, or running,
//! plus the two global-option facts that need a real process to observe.
//!
//! Assertions are on the exit code and on `--json`, never on the human table, which §10
//! leaves free to change. The one exception is deliberate: the "no escapes when piped"
//! case *is* about the human output, and asserting it anywhere else would be asserting
//! nothing.

mod common;

use common::{PrivateBus, skipped};

/// A stand-in for `Manager1`, serving the one method `status` calls.
///
/// Not the daemon: what is under test here is the client's reading of the bus, and a
/// real daemon would bring its own reasons to fail (a backend probe, a store) into a
/// test about `NameHasOwner`. The daemon end is [8.11].
///
/// [8.11]: https://github.com/jeanparpaillon/scanbus_service/issues/39
struct Manager;

#[zbus::interface(name = "org.scanbus.Manager1")]
impl Manager {
    #[zbus(property)]
    fn version(&self) -> String {
        "1.2.3".to_owned()
    }

    #[zbus(property)]
    fn backends(&self) -> Vec<String> {
        vec!["mobile".to_owned(), "brother".to_owned()]
    }

    fn get_profile_types(&self) -> Vec<String> {
        vec!["image".to_owned(), "document".to_owned()]
    }
}

/// Owns `org.scanbus` on `address` until the returned connection is dropped.
async fn serve(address: &str) -> zbus::Connection {
    zbus::connection::Builder::address(address)
        .expect("the private bus address must parse")
        .name("org.scanbus")
        .expect("org.scanbus is a well-known name")
        .serve_at("/org/scanbus", Manager)
        .expect("/org/scanbus is a valid path")
        .build()
        .await
        .expect("cannot own org.scanbus on the private bus")
}

/// Acceptance: no daemon and no activation file — exit 3, and the report says so.
#[test]
fn an_absent_daemon_is_reported_and_exits_three() {
    let Some(bus) = PrivateBus::start() else {
        return skipped("an_absent_daemon_is_reported_and_exits_three");
    };

    let run = bus.scanbus(&["--json", "status"]);
    run.assert_code(3);

    // The word on stdout, and on stderr the two facts behind it — which is what tells a
    // user whether to start the daemon or to install it.
    assert!(
        run.stderr.contains("nothing owns org.scanbus") && run.stderr.contains("not activatable"),
        "{}",
        run.stderr
    );

    let status = run.json();
    assert_eq!(status["daemon"], "absent");
    assert!(status["owner"].is_null(), "{status}");
    assert_eq!(status["profiles"], serde_json::json!([]));
    assert_eq!(status["name"], "org.scanbus");
}

/// Acceptance: with the activation file installed, `status` reports `activatable` and
/// exits 0 — **without** starting anything.
///
/// The activation file points at `/bin/false`, so a `status` that made a method call
/// would fail the activation and report something else entirely. That is the assertion:
/// the answer is `activatable` and there is still no owner afterwards.
#[test]
fn an_activatable_daemon_exits_zero_and_is_not_started() {
    let Some(bus) = PrivateBus::start_with_activation(true) else {
        return skipped("an_activatable_daemon_exits_zero_and_is_not_started");
    };

    let status = bus.scanbus(&["--json", "status"]).assert_code(0).json();
    assert_eq!(status["daemon"], "activatable");
    assert!(status["owner"].is_null(), "{status}");

    // Run it again: had the first run activated, the second would see the result of it
    // — a failed spawn, or an owner. It sees neither.
    let status = bus.scanbus(&["--json", "status"]).assert_code(0).json();
    assert_eq!(status["daemon"], "activatable");
}

/// A daemon that is there: the owner, the global properties and the profile types come
/// back.
#[tokio::test(flavor = "multi_thread")]
async fn a_running_daemon_reports_its_owner_properties_and_profile_types() {
    let Some(bus) = PrivateBus::start() else {
        return skipped("a_running_daemon_reports_its_owner_and_its_profile_types");
    };
    let _daemon = serve(bus.address()).await;

    let status = bus.scanbus(&["--json", "status"]).assert_code(0).json();

    assert_eq!(status["daemon"], "running");
    assert_eq!(status["version"], "1.2.3");
    assert_eq!(status["backends"], serde_json::json!(["mobile", "brother"]));
    assert_eq!(status["profiles"], serde_json::json!(["image", "document"]));
    let owner = status["owner"]
        .as_str()
        .expect("a running daemon has an owner");
    assert!(owner.starts_with(':'), "{owner} is not a unique name");
}

/// Acceptance: `scanbus … | cat` emits no ANSI escapes.
///
/// `Command::output` gives the child a pipe for stdout, which is the "not a terminal"
/// branch of §3's precedence list — the same thing `| cat` does. The colour on a real
/// terminal is unit-tested in `output`, since a test cannot allocate a TTY without a
/// pty.
#[test]
fn piped_output_carries_no_escapes() {
    let Some(bus) = PrivateBus::start() else {
        return skipped("piped_output_carries_no_escapes");
    };

    let run = bus.scanbus(&["status"]);
    run.assert_code(3);
    assert!(!run.has_escapes(), "escapes in a pipe: {:?}", run.stdout);
    assert!(run.stdout.contains("absent"), "{}", run.stdout);
}

/// Acceptance: `--no-activate` against a stopped daemon exits 3, for a command that is
/// not `status`.
///
/// `list` is a stub, and that is what makes this the right assertion to have now: the
/// connection is established before dispatch, so the answer to "is there a daemon"
/// cannot depend on how finished the command is.
#[test]
fn no_activate_against_a_stopped_daemon_exits_three() {
    let Some(bus) = PrivateBus::start_with_activation(true) else {
        return skipped("no_activate_against_a_stopped_daemon_exits_three");
    };

    let run = bus.scanbus(&["--no-activate", "list"]);
    run.assert_code(3);
    assert!(
        run.stderr.contains("org.scanbus") && run.stderr.contains("activation was disabled"),
        "{}",
        run.stderr
    );
    assert!(run.stdout.is_empty(), "errors go to stderr: {}", run.stdout);
}

/// Acceptance: `scanbus` with no arguments prints help and exits 2.
///
/// No bus: a usage error is decided before anything is connected, and that is the point
/// — it costs nothing and works on a machine with no D-Bus at all.
#[test]
fn no_arguments_prints_help_and_exits_two() {
    let run = common::scanbus(&[]);

    run.assert_code(2);
    assert!(run.stderr.contains("Usage: scanbus"), "{}", run.stderr);
    assert!(run.stderr.contains("status"), "{}", run.stderr);
}

/// `--timeout` is a ceiling on the call, and a bad one is a usage error rather than a
/// surprise thirty seconds later.
#[test]
fn a_malformed_timeout_is_a_usage_error_naming_the_units() {
    let run = common::scanbus(&["--timeout", "soon", "status"]);

    run.assert_code(2);
    assert!(run.stderr.contains("ms, s, m, h"), "{}", run.stderr);
}
