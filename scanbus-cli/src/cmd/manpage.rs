//! `scanbus manpage` — generate the `scanbus(1)` man page locally.

use clap::CommandFactory as _;
use clap_mangen::Man;

use crate::cli::Cli;
use crate::error::Result;

/// Writes the man page to stdout and exits 0.
pub fn run() -> Result<u8> {
    let command = Cli::command();
    let man = Man::new(command);
    let mut stdout = std::io::stdout().lock();
    man.render(&mut stdout)
        .map_err(crate::error::Error::write)?;
    Ok(0)
}
