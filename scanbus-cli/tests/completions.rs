//! `scanbus completions …` emits shell completion scripts without touching D-Bus.

mod common;

use std::process::Command;

/// Acceptance: the Bash generator exits 0 and prints a Bash completion function.
#[test]
fn bash_completions_print_without_needing_a_bus() {
    let run = common::scanbus(&["completions", "bash"]);

    run.assert_code(0);
    assert!(run.stderr.is_empty(), "{}", run.stderr);
    assert!(run.stdout.contains("_scanbus"), "{}", run.stdout);
    assert!(run.stdout.contains("complete -F"), "{}", run.stdout);
}

/// Loading the generated script with the usual quoted `eval "$(…)"` form works in Bash.
#[test]
fn bash_completions_can_be_evaled_when_quoted() {
    let output = Command::new("bash")
        .arg("-lc")
        .arg("eval \"$($CARGO_BIN_EXE_scanbus completions bash)\"; complete -p scanbus")
        .env("CARGO_BIN_EXE_scanbus", env!("CARGO_BIN_EXE_scanbus"))
        .output()
        .expect("cannot run bash");

    assert!(
        output.status.success(),
        "exit {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is not UTF-8");
    assert!(stdout.contains("_scanbus"), "{stdout}");
}
