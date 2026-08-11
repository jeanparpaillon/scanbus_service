//! `scanbus completions …` emits shell completion scripts without touching D-Bus.

mod common;

/// Acceptance: the Bash generator exits 0 and prints a Bash completion function.
#[test]
fn bash_completions_print_without_needing_a_bus() {
    let run = common::scanbus(&["completions", "bash"]);

    run.assert_code(0);
    assert!(run.stderr.is_empty(), "{}", run.stderr);
    assert!(run.stdout.contains("_scanbus"), "{}", run.stdout);
    assert!(run.stdout.contains("complete -F"), "{}", run.stdout);
}
