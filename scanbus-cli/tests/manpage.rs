//! `scanbus manpage` emits a `scanbus(1)` document without touching D-Bus.

mod common;

/// Acceptance: the generator exits 0 and prints the roff header for `scanbus(1)`.
#[test]
fn manpage_prints_without_needing_a_bus() {
    let run = common::scanbus(&["manpage"]);

    run.assert_code(0);
    assert!(run.stderr.is_empty(), "{}", run.stderr);
    assert!(run.stdout.contains(".TH scanbus 1"), "{}", run.stdout);
    assert!(run.stdout.contains(".SH NAME"), "{}", run.stdout);
}
