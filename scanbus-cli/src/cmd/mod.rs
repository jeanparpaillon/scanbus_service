//! One module per command group, and the dispatch that stands between them and `main`.
//!
//! Everything but [`status`] is a stub at this point, and the stubs are not empty: they
//! **open the connection first**, then fail. That order is the whole of what 8.2 can
//! test about the other commands, and it is the order every implementation must keep —
//! `scanbus --no-activate list` against a stopped daemon has to exit 3 because the
//! daemon is stopped, not exit 1 because the command is unfinished, and a stub that
//! failed before connecting would let that regress unnoticed until 8.5 lands.
//!
//! `status` is the exception in the other direction: it is the only command that does
//! not go through [`Context::connect`], because refusing when the name has no owner is
//! precisely the case it exists to report.

pub mod status;

use crate::cli::Command;
use crate::context::Context;
use crate::error::{Error, Result};

/// Runs one command, returning the process exit code for a *successful* run.
///
/// A code rather than `()` because success is not always 0: `status` reports an absent
/// daemon by exiting 3 with a report on stdout and nothing on stderr, which is a
/// different thing from failing ([`scanbus-cli.md`] §3).
///
/// # Errors
///
/// [`Error`], whose [`exit_code`](Error::exit_code) is what the process ends with.
///
/// [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md
pub async fn dispatch(context: &Context, command: &Command) -> Result<u8> {
    match command {
        Command::Status => status::run(context).await,
        pending => stub(context, pending).await,
    }
}

/// Connects the way the finished command will, then says which issue finishes it.
async fn stub(context: &Context, command: &Command) -> Result<u8> {
    let (name, issue) = pending(command);

    // Dropped immediately, and still worth making: it is what turns "no daemon" into
    // exit 3 for every command in the surface, today, rather than one issue at a time.
    let _connection = context.connect().await?;

    Err(Error::not_implemented(name, issue))
}

/// The command as a user typed it, and the issue that implements it.
///
/// A table rather than a single "not implemented": the issues are what makes the failure
/// actionable, and writing them down here is also a checklist of what workstream 8 has
/// left — a command missing from this list stops compiling the moment it is added to
/// [`Command`].
const fn pending(command: &Command) -> (&'static str, &'static str) {
    use crate::cli::{ButtonCommand, JobCommand, ProfileCommand};

    match command {
        // 8.2 owns `status`, which is implemented; the arm exists so that adding a
        // command to `Command` is a compile error here rather than a silent fallthrough.
        Command::Status => ("status", "8.2"),
        Command::List { .. } => ("list", "8.5"),
        Command::Show { .. } => ("show", "8.5"),
        Command::Discover { .. } => ("discover", "8.5"),
        Command::Pair { .. } => ("pair", "8.6"),
        Command::CancelPairing { .. } => ("cancel-pairing", "8.6"),
        Command::Unpair { .. } => ("unpair", "8.6"),
        Command::Connect { .. } => ("connect", "8.7"),
        Command::Disconnect { .. } => ("disconnect", "8.7"),
        Command::Scan { .. } => ("scan", "8.7"),
        Command::Button { command } => match command {
            ButtonCommand::List { .. } => ("button list", "8.9"),
            ButtonCommand::Set { .. } => ("button set", "8.9"),
            ButtonCommand::Clear { .. } => ("button clear", "8.9"),
        },
        Command::Job { command } => match command {
            JobCommand::List { .. } => ("job list", "8.8"),
            JobCommand::Show { .. } => ("job show", "8.8"),
            JobCommand::Watch { .. } => ("job watch", "8.8"),
        },
        Command::Profile { command } => match command {
            ProfileCommand::List => ("profile list", "8.10"),
            ProfileCommand::Show { .. } => ("profile show", "8.10"),
            ProfileCommand::Set { .. } => ("profile set", "8.10"),
        },
        Command::Monitor { .. } => ("monitor", "8.8"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    use crate::cli::Cli;

    fn command(args: &[&str]) -> Command {
        Cli::try_parse_from(args).unwrap().command
    }

    /// The name in the message is the one the user typed, group included — `scanbus
    /// button set`, not `scanbus set`.
    #[test]
    fn a_stub_names_the_command_as_typed_and_the_issue_that_finishes_it() {
        for (args, expected) in [
            (vec!["scanbus", "list"], ("list", "8.5")),
            (vec!["scanbus", "connect", "MFC"], ("connect", "8.7")),
            (
                vec!["scanbus", "button", "set", "MFC", "2"],
                ("button set", "8.9"),
            ),
            (vec!["scanbus", "profile", "list"], ("profile list", "8.10")),
            (vec!["scanbus", "monitor"], ("monitor", "8.8")),
        ] {
            assert_eq!(pending(&command(&args)), expected, "{args:?}");
        }
    }
}
