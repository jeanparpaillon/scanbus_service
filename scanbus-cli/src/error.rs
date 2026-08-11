//! What a command can end as, and the exit code that goes with it.
//!
//! [`scanbus-cli.md`] §8 fixes the table, and most of it is not decided here: the seven
//! named errors of the D-Bus API carry their own code through
//! [`ScanbusError::exit_code`], which lives next to the enum in `scanbus-client` so that
//! the daemon's half of the contract ([2.7]) can be enumerated against it. What this
//! module adds is the part that is the *client's* fact rather than the daemon's:
//!
//! - **the sentence a user reads**, which §6 fixes as
//!   `scanbus: <what failed>: <D-Bus error name>: <message>`. "What failed" is context
//!   only the call site has, so every error carries it;
//! - **a selector, which never reached the daemon**. [`ScanbusError`] cannot describe
//!   "you named something that does not exist", because nothing was called: the daemon
//!   answered `GetManagedObjects` and it is this client that then refused. §8 gives that
//!   exit 4, along with the object that resolved and was gone by the time the call
//!   reached it — same fact, later.
//! - **activation, which is not a refusal**. `org.freedesktop.DBus.Error.ServiceUnknown`
//!   and the `…Error.Spawn.*` family arrive as ordinary named errors and would map to
//!   exit 1, but §8 gives "daemon unavailable, or activation failed" its own code (3),
//!   and a script that cannot tell "the daemon is not installed" from "the daemon said
//!   no" has to parse English to retry correctly. The bus's own text is kept verbatim in
//!   the message, because for a failed activation it is the only thing that names the
//!   unit or the binary that did not start.
//!
//! [2.7]: https://github.com/jeanparpaillon/scanbus_service/issues/11
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

use std::fmt;
use std::time::Duration;

use scanbus_client::{Error as ClientError, ScanbusError};

/// A failed command, with the phrase that says what was being attempted.
#[derive(Debug)]
pub struct Error {
    /// What failed, as a gerund phrase: `"reading the profile types"`.
    what: String,
    kind: Kind,
}

/// The kinds of failure that reach `main`.
#[derive(Debug)]
enum Kind {
    /// Anything the client crate reports: a refusal, a transport failure, a value that
    /// did not decode, or `--no-activate` against a daemon that is not running.
    Client(ClientError),

    /// `--timeout` elapsed with no reply.
    ///
    /// Deliberately *not* exit 12. §8 gives 12 to "timed out waiting for the state a
    /// `--wait` was told to wait for", which is a statement about the scanner; a call
    /// that never came back is a statement about the daemon, and folding the two would
    /// make exit 12 unusable for the thing it names.
    Timeout(Duration),

    /// A `--wait` — in practice, `pair` — whose target never reached the state being
    /// waited for before `--timeout` elapsed.
    ///
    /// The daemon kept answering; the scanner just never got to `done` or `failed`, so
    /// this is exit 12 rather than [`Kind::Timeout`]'s 1: "the daemon never told me it
    /// finished" is a different fact from "the daemon never replied".
    WaitTimeout(Duration),

    /// A command whose D-Bus surface exists but whose implementation is a later issue,
    /// named so the message is a pointer rather than a wall.
    NotImplemented(String),

    /// Writing the output failed — a closed pipe, most often.
    Write(std::io::Error),
}

impl Error {
    /// A failure from the client crate, in the context it happened in.
    pub fn call(what: impl Into<String>, error: ClientError) -> Self {
        Self {
            what: what.into(),
            kind: Kind::Client(error),
        }
    }

    /// `--timeout` elapsed while doing `what`.
    pub fn timeout(what: impl Into<String>, after: Duration) -> Self {
        Self {
            what: what.into(),
            kind: Kind::Timeout(after),
        }
    }

    /// `--timeout` elapsed while waiting for a scanner to reach the state `what` names.
    pub fn wait_timeout(what: impl Into<String>, after: Duration) -> Self {
        Self {
            what: what.into(),
            kind: Kind::WaitTimeout(after),
        }
    }

    /// A subcommand that parses and dispatches but does not do anything yet.
    ///
    /// It exists as an error rather than as a silent success so that a script written
    /// against a half-built CLI fails now instead of appearing to work: exit 1 with a
    /// sentence naming the issue that will implement it.
    pub fn not_implemented(command: &str, issue: &str) -> Self {
        Self {
            what: format!("running `scanbus {command}`"),
            kind: Kind::NotImplemented(issue.to_owned()),
        }
    }

    /// Failed output, which is a failure of the command like any other.
    pub fn write(error: std::io::Error) -> Self {
        Self {
            what: "writing the output".to_owned(),
            kind: Kind::Write(error),
        }
    }

    /// The process exit code, per §8.
    pub fn exit_code(&self) -> u8 {
        match &self.kind {
            // §8 code 3, and the reason the variant exists separately in the client.
            Kind::Client(ClientError::NotRunning) => 3,
            Kind::Client(ClientError::Call(error)) if is_activation_failure(error) => 3,
            Kind::Client(ClientError::Call(error)) => error.exit_code(),
            // §8 code 4. `Vanished` shares it with `Select` because it is the same fact
            // arriving later: the object the user named is not there. A script retrying
            // after a `discover` wants both cases, and neither is the daemon refusing.
            Kind::Client(ClientError::Select(_) | ClientError::Vanished { .. }) => 4,
            Kind::WaitTimeout(_) => 12,
            Kind::Client(_) | Kind::Timeout(_) | Kind::NotImplemented(_) | Kind::Write(_) => 1,
        }
    }
}

impl fmt::Display for Error {
    /// `<what failed>: <detail>`; `main` prefixes the program name.
    ///
    /// For a named refusal the detail is `<name>: <message>` — [`ScanbusError`]'s own
    /// `Display` — which is what makes the §6 shape hold for every error, including the
    /// ones this client has never heard of.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: ", self.what)?;

        match &self.kind {
            Kind::Client(error) => write!(f, "{error}"),
            Kind::Timeout(after) => write!(f, "no reply after {}", format_duration(*after)),
            Kind::WaitTimeout(after) => {
                write!(f, "still not done after {}", format_duration(*after))
            }
            Kind::NotImplemented(issue) => {
                write!(f, "not implemented yet — issue {issue} builds it")
            }
            Kind::Write(error) => write!(f, "{error}"),
        }
    }
}

/// Whether this refusal came from the bus failing to *start* the daemon.
///
/// `ServiceUnknown` is "the name has no owner and no activation file"; the `Spawn.*`
/// family is every way an activation that was attempted can fail — a unit that exits
/// non-zero, a binary that is not there, a service file that does not parse. All of them
/// mean the same thing to a caller: there is no daemon to talk to, and it is not because
/// the daemon refused.
fn is_activation_failure(error: &ScanbusError) -> bool {
    let name = error.name();
    name == "org.freedesktop.DBus.Error.ServiceUnknown"
        || name.starts_with("org.freedesktop.DBus.Error.Spawn.")
}

/// A duration the way `--timeout` would have been written.
fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis.is_multiple_of(1_000) {
        format!("{}s", millis / 1_000)
    } else {
        format!("{millis}ms")
    }
}

/// The result of a command.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use scanbus_client::{ObjectKind, SelectError};

    use super::*;

    fn refusal(name: &str) -> Error {
        Error::call(
            "connecting the scanner",
            ClientError::Call(ScanbusError::from_reply(name, "detail")),
        )
    }

    /// The named errors keep the code the client crate gives them: this end of the CLI
    /// must not become a second table that can disagree with §8.
    #[test]
    fn a_named_refusal_keeps_the_client_crates_code() {
        assert_eq!(refusal("org.scanbus.Error.NotReachable").exit_code(), 5);
        assert_eq!(refusal("org.scanbus.Error.NotPaired").exit_code(), 7);
        assert_eq!(
            refusal("org.scanbus.Error.UnsupportedProfile").exit_code(),
            10
        );
    }

    /// Activation is the one place the CLI overrides that mapping, and the override is
    /// what makes "the daemon is not installed" scriptable.
    #[test]
    fn a_failed_activation_is_exit_three_and_not_exit_one() {
        for name in [
            "org.freedesktop.DBus.Error.ServiceUnknown",
            "org.freedesktop.DBus.Error.Spawn.ExecFailed",
            "org.freedesktop.DBus.Error.Spawn.ChildExited",
            "org.freedesktop.DBus.Error.Spawn.ServiceNotFound",
        ] {
            let error = refusal(name);
            assert_eq!(error.exit_code(), 3, "{name}");
            assert!(
                error.to_string().contains(name) && error.to_string().contains("detail"),
                "the bus's own text must survive verbatim: {error}"
            );
        }
    }

    /// A refusal that merely comes from the standard family is still exit 1: only the
    /// two activation shapes are special.
    #[test]
    fn another_standard_error_is_still_unclassified() {
        assert_eq!(
            refusal("org.freedesktop.DBus.Error.InvalidArgs").exit_code(),
            1
        );
        assert_eq!(
            refusal("org.freedesktop.DBus.Error.UnknownMethod").exit_code(),
            1
        );
    }

    /// `--no-activate` against a stopped daemon, which is exit 3 by a different route.
    #[test]
    fn a_daemon_that_may_not_be_started_is_exit_three() {
        let error = Error::call("listing the scanners", ClientError::NotRunning);
        assert_eq!(error.exit_code(), 3);
        assert!(error.to_string().contains("org.scanbus"), "{error}");
    }

    /// A selector that named nothing, or too much, is exit 4 — and the sentence carries
    /// the candidates, because an ambiguity report without them is a riddle.
    #[test]
    fn a_selector_that_did_not_resolve_is_exit_four() {
        let ambiguous = SelectError::Ambiguous {
            kind: ObjectKind::Scanner,
            selector: "MFC".to_owned(),
            candidates: vec!["brother_a (MFC-L2710DW)".to_owned(), "brother_b".to_owned()],
        };
        let error = Error::call("finding the scanner", ClientError::from(ambiguous));

        assert_eq!(error.exit_code(), 4);
        let printed = error.to_string();
        assert!(printed.starts_with("finding the scanner: "), "{printed}");
        assert!(printed.contains("brother_a (MFC-L2710DW)"), "{printed}");
        assert!(printed.contains("brother_b"), "{printed}");

        let missing = SelectError::NotFound {
            kind: ObjectKind::Job,
            selector: "4f2a".to_owned(),
            known: Vec::new(),
        };
        assert_eq!(
            Error::call("finding the job", ClientError::from(missing)).exit_code(),
            4
        );
    }

    /// An object that resolved and then left the bus is the same exit code: it is still
    /// "the thing you named is not there", and it must not read as a `zbus` failure.
    #[test]
    fn an_object_that_vanished_is_exit_four_and_names_itself() {
        let error = Error::call(
            "pairing the scanner",
            ClientError::Vanished {
                kind: ObjectKind::Scanner,
                name: "brother_net_192_2E168_2E1_2E23".to_owned(),
            },
        );

        assert_eq!(error.exit_code(), 4);
        assert!(
            error.to_string().contains("brother_net_192_2E168_2E1_2E23"),
            "{error}"
        );
    }

    /// A call that never came back is not the wait timeout of exit 12.
    #[test]
    fn a_call_timeout_is_exit_one_and_says_how_long_it_waited() {
        let error = Error::timeout("reading the profile types", Duration::from_secs(30));
        assert_eq!(error.exit_code(), 1);
        assert_eq!(
            error.to_string(),
            "reading the profile types: no reply after 30s"
        );

        let error = Error::timeout("x", Duration::from_millis(1_500));
        assert!(error.to_string().ends_with("1500ms"), "{error}");
    }

    /// A stub says which issue owns it, so the message is a pointer rather than a wall.
    #[test]
    fn a_stub_names_the_issue_that_will_implement_it() {
        let error = Error::not_implemented("list", "8.5");
        assert_eq!(error.exit_code(), 1);
        assert_eq!(
            error.to_string(),
            "running `scanbus list`: not implemented yet — issue 8.5 builds it"
        );
    }
}
