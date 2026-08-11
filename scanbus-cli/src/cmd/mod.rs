//! One module per command group, and the dispatch that stands between them and `main`.
//!
//! [`status`] ([8.2](https://github.com/jeanparpaillon/scanbus_service/issues/29)) and
//! `list`/`show`/`discover` ([8.5](https://github.com/jeanparpaillon/scanbus_service/issues/32),
//! in [`list`], [`show`], [`discover`]) are implemented. Everything else is still a stub,
//! and the stubs are not empty: they **open the connection first**, then **resolve
//! whatever selectors the command carries**, and only then fail. That order is the whole
//! of what the finished commands must keep — `scanbus --no-activate pair MFC` against a
//! stopped daemon has to exit 3 because the daemon is stopped, and `scanbus connect
//! nosuch` has to exit 4 because nothing is called `nosuch`, neither of them exit 1
//! because the command is unfinished. A stub that failed earlier would let both regress
//! unnoticed until the issue that finishes the command lands.
//!
//! Resolving in the stub is also what makes §5 observable end to end today: the selector
//! table itself is unit-tested in `scanbus-client` without a bus, and the exit code, the
//! message and the promise that resolution never starts discovery are asserted against a
//! real process in `tests/select.rs`.
//!
//! `status` is the exception in the other direction: it is the only command that does
//! not go through [`Context::connect`], because refusing when the name has no owner is
//! precisely the case it exists to report.

pub mod status;

mod button;
mod completions;
mod connect;
mod discover;
mod job;
mod job_follow;
mod list;
mod manpage;
mod monitor;
mod options;
mod pair;
mod profile;
mod scanner_view;
mod show;

use scanbus_client::{Connection, Match, Objects, Scanner};

use crate::cli::{ButtonCommand, Command, JobCommand, ProfileCommand, ScannerArg};
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
#[allow(
    unreachable_patterns,
    reason = "the stub arm stays as the future-command checklist even when all current commands are implemented"
)]
pub async fn dispatch(context: &Context, command: &Command) -> Result<u8> {
    match command {
        Command::Completions { shell } => completions::run(*shell),
        Command::Manpage => manpage::run(),
        Command::Status => status::run(context).await,
        Command::List { paired, unpaired } => list::run(context, *paired, *unpaired).await,
        Command::Show { scanner } => show::run(context, scanner).await,
        Command::Discover {
            backends,
            duration,
            watch,
            keep,
        } => discover::run(context, backends, *duration, *watch, *keep).await,
        Command::Pair { scanner, no_wait } => pair::run(context, scanner, *no_wait).await,
        Command::CancelPairing { scanner } => pair::cancel(context, scanner).await,
        Command::Unpair { scanner, yes } => pair::unpair(context, scanner, *yes).await,
        Command::Connect { scanner, profile } => connect::connect(context, scanner, profile).await,
        Command::Disconnect { scanner } => connect::disconnect(context, scanner).await,
        Command::Scan {
            scanner,
            profile,
            options,
            no_wait,
        } => connect::scan(context, scanner, profile, options, *no_wait).await,
        Command::Button { command } => match command {
            ButtonCommand::List { scanner } => button::list(context, scanner).await,
            ButtonCommand::Set {
                scanner,
                button,
                profile,
                label,
                options,
                option_json,
            } => {
                button::set(
                    context,
                    scanner,
                    button,
                    profile,
                    label,
                    options,
                    option_json,
                )
                .await
            }
            ButtonCommand::Clear { scanner, button } => {
                button::clear(context, scanner, button).await
            }
        },
        Command::Job { command } => match command {
            JobCommand::List { filter } => job::list(context, filter).await,
            JobCommand::Show { job } => job::show(context, job).await,
            JobCommand::Watch { filter, until_done } => {
                job::watch(context, filter, *until_done).await
            }
        },
        Command::Monitor { path } => monitor::run(context, path.as_deref()).await,
        Command::Profile { command } => match command {
            ProfileCommand::List => profile::list(context).await,
            ProfileCommand::Show { name } => profile::show(context, name).await,
            ProfileCommand::Set {
                name,
                options,
                option_json,
            } => profile::set(context, name, options, option_json).await,
        },
        pending => stub(context, pending).await,
    }
}

/// Connects and resolves the way the finished command will, then says which issue
/// finishes it.
async fn stub(context: &Context, command: &Command) -> Result<u8> {
    let (name, issue) = pending(command);

    let connection = context.connect().await?;
    resolve(context, &connection, command).await?;

    Err(Error::not_implemented(name, issue))
}

/// Resolves every selector `command` carries, against one snapshot of the object tree.
///
/// One `GetManagedObjects` for the whole command rather than one per selector: `button
/// set MFC 2` resolves the key from the same reply the scanner came out of, so the two
/// answers cannot be from two different moments — and a scanner that disappeared between
/// them cannot yield a button belonging to nothing.
///
/// # Errors
///
/// [`Error`] with exit 4 for a selector that matched nothing or several things, and
/// whatever reading the object tree failed with otherwise.
async fn resolve(context: &Context, connection: &Connection, command: &Command) -> Result<()> {
    let Some(selectors) = selectors(command) else {
        return Ok(());
    };

    // Read once, then decide: everything below is pure matching against this snapshot.
    let objects = context
        .within("listing the daemon's objects", Objects::fetch(connection))
        .await?;

    match selectors {
        Selectors::Scanner(argument) => {
            scanner(&objects, argument)?;
        }
        Selectors::ScannerButton(argument, button) => {
            let found = scanner(&objects, argument)?;
            objects
                .button(&found.id, button)
                .map_err(|error| Error::call("finding the button", error.into()))?;
        }
        Selectors::Job(selector) => {
            objects
                .job(selector)
                .map_err(|error| Error::call("finding the job", error.into()))?;
        }
        Selectors::ScannerFilter(selector, matching) => {
            objects
                .scanner(selector, matching)
                .map_err(|error| Error::call("finding the scanner", error.into()))?;
        }
    }

    Ok(())
}

/// Resolves one [`ScannerArg`], honouring its `--id`.
fn scanner<'a>(objects: &'a Objects, argument: &ScannerArg) -> Result<&'a Scanner> {
    objects
        .scanner(&argument.scanner, argument.matching())
        .map_err(|error| Error::call("finding the scanner", error.into()))
}

/// What a command names, in the order the objects have to be resolved in.
enum Selectors<'a> {
    /// One scanner.
    Scanner(&'a ScannerArg),
    /// One scanner, then one of its buttons.
    ScannerButton(&'a ScannerArg, &'a str),
    /// One job, which names its scanner through its path.
    Job(&'a str),
    /// The optional `--scanner` of a `job` listing.
    ScannerFilter(&'a str, Match),
}

/// The selectors of a command, or `None` for one that names no object.
///
/// Exhaustive over [`Command`] on purpose, like [`pending`] below: a command that grows
/// an argument taking a selector should stop compiling here rather than quietly skip
/// resolution.
fn selectors(command: &Command) -> Option<Selectors<'_>> {
    match command {
        Command::Show { scanner }
        | Command::Pair { scanner, .. }
        | Command::CancelPairing { scanner }
        | Command::Unpair { scanner, .. }
        | Command::Connect { scanner, .. }
        | Command::Disconnect { scanner }
        | Command::Scan { scanner, .. } => Some(Selectors::Scanner(scanner)),

        Command::Button { command } => match command {
            ButtonCommand::List { scanner } => Some(Selectors::Scanner(scanner)),
            ButtonCommand::Set {
                scanner, button, ..
            }
            | ButtonCommand::Clear { scanner, button } => {
                Some(Selectors::ScannerButton(scanner, button))
            }
        },

        Command::Job { command } => match command {
            JobCommand::Show { job } => Some(Selectors::Job(job)),
            JobCommand::List { filter } | JobCommand::Watch { filter, .. } => filter
                .scanner
                .as_deref()
                .map(|selector| Selectors::ScannerFilter(selector, filter.matching())),
        },

        // `list` and `discover` report what exists rather than naming it, a `profile`
        // object is named by one of the four fixed names of §6, and `monitor --path` is a
        // path prefix rather than a selector — a prefix that matches nothing is an empty
        // stream, not a failed lookup.
        Command::Completions { .. }
        | Command::Manpage
        | Command::Status
        | Command::List { .. }
        | Command::Discover { .. }
        | Command::Profile { .. }
        | Command::Monitor { .. } => None,
    }
}

/// The command as a user typed it, and the issue that implements it.
///
/// A table rather than a single "not implemented": the issues are what makes the failure
/// actionable, and writing them down here is also a checklist of what workstream 8 has
/// left — a command missing from this list stops compiling the moment it is added to
/// [`Command`].
const fn pending(command: &Command) -> (&'static str, &'static str) {
    match command {
        // `completions` is implemented; the arm exists so new commands still have to say
        // whether they are implemented or stubs.
        Command::Completions { .. } => ("completions", "8.11"),
        Command::Manpage => ("manpage", "8.11"),
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
                vec![
                    "scanbus",
                    "button",
                    "set",
                    "MFC",
                    "2",
                    "--profile",
                    "document",
                ],
                ("button set", "8.9"),
            ),
            (vec!["scanbus", "profile", "list"], ("profile list", "8.10")),
            (vec!["scanbus", "monitor"], ("monitor", "8.8")),
        ] {
            assert_eq!(pending(&command(&args)), expected, "{args:?}");
        }
    }

    /// Which commands resolve what. The interesting rows are the negative ones: a command
    /// that reads the object tree without naming anything in it must not go through
    /// resolution, or `scanbus list` against a daemon with no scanners would exit 4.
    #[test]
    fn a_command_resolves_exactly_the_selectors_it_carries() {
        for (args, expected) in [
            (vec!["scanbus", "show", "MFC"], Some("scanner")),
            (vec!["scanbus", "unpair", "MFC", "--yes"], Some("scanner")),
            (vec!["scanbus", "button", "list", "MFC"], Some("scanner")),
            (
                vec![
                    "scanbus",
                    "button",
                    "set",
                    "MFC",
                    "2",
                    "--profile",
                    "document",
                ],
                Some("scanner+button"),
            ),
            (
                vec!["scanbus", "button", "clear", "MFC", "2"],
                Some("scanner+button"),
            ),
            (vec!["scanbus", "job", "show", "4"], Some("job")),
            (
                vec!["scanbus", "job", "list", "--scanner", "MFC"],
                Some("filter"),
            ),
            (
                vec!["scanbus", "job", "watch", "--scanner", "MFC"],
                Some("filter"),
            ),
            // No selector: nothing to resolve, and nothing to fail on.
            (vec!["scanbus", "completions", "bash"], None),
            (vec!["scanbus", "manpage"], None),
            (vec!["scanbus", "list"], None),
            (vec!["scanbus", "discover"], None),
            (vec!["scanbus", "job", "list"], None),
            (vec!["scanbus", "job", "watch", "--until-done"], None),
            (vec!["scanbus", "profile", "show", "document"], None),
            (vec!["scanbus", "monitor", "--path", "/org/scanbus"], None),
        ] {
            let command = command(&args);
            let named = selectors(&command).map(|selectors| match selectors {
                Selectors::Scanner(_) => "scanner",
                Selectors::ScannerButton(..) => "scanner+button",
                Selectors::Job(_) => "job",
                Selectors::ScannerFilter(..) => "filter",
            });

            assert_eq!(named, expected, "{args:?}");
        }
    }

    /// `--id` reaches the resolver as the mode that refuses the other three spellings,
    /// wherever on the line it was written.
    #[test]
    fn the_id_flag_pins_the_selector_to_an_exact_id() {
        for (args, expected) in [
            (vec!["scanbus", "show", "MFC"], Match::Any),
            (vec!["scanbus", "show", "--id", "MFC"], Match::ExactId),
            (vec!["scanbus", "show", "MFC", "--id"], Match::ExactId),
        ] {
            let command = command(&args);
            let Some(Selectors::Scanner(argument)) = selectors(&command) else {
                panic!("{args:?} names a scanner");
            };
            assert_eq!(argument.matching(), expected, "{args:?}");
        }

        let command = command(&["scanbus", "job", "list", "--scanner", "MFC", "--id"]);
        let Some(Selectors::ScannerFilter(selector, matching)) = selectors(&command) else {
            panic!("--scanner is a selector");
        };
        assert_eq!((selector, matching), ("MFC", Match::ExactId));
    }

    /// `--id` with nothing to qualify is a usage error, not a flag that does nothing.
    #[test]
    fn the_id_flag_of_a_job_listing_requires_a_scanner() {
        assert!(Cli::try_parse_from(["scanbus", "job", "list", "--id"]).is_err());
        assert!(Cli::try_parse_from(["scanbus", "job", "watch", "--id"]).is_err());
    }
}
