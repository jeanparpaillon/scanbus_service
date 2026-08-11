//! `scanbus completions <shell>` — generate shell completion scripts locally.

use clap::CommandFactory as _;
use clap_complete::{Shell, generate};

use crate::cli::Cli;
use crate::error::Result;

/// Writes the completion script to stdout and exits 0.
pub fn run(shell: Shell) -> Result<u8> {
    let mut command = Cli::command();
    let name = command.get_name().to_owned();
    generate(shell, &mut command, name, &mut std::io::stdout());
    Ok(0)
}
