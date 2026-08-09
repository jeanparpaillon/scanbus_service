//! `scanbus status` — is there a daemon, and what does it say about itself.
//!
//! The one command that is allowed to find no daemon, and the one that must not create
//! one. Both follow from what it is for: a health check that can be run from a script or
//! a monitoring hook without side effects. So it asks the *bus* — `NameHasOwner`,
//! `ListActivatableNames`, `GetNameOwner`, all of them calls to `org.freedesktop.DBus`
//! itself — and only calls the daemon once the name already has an owner. `scanbus
//! status` on a machine where the daemon is installed but stopped therefore leaves it
//! stopped, which is the property a health check has to have to be usable at all.
//!
//! # `Version` and `Backends` are `-`, and that is a gap in the contract
//!
//! §2 of the D-Bus API gives `Manager1` three methods and no properties, so there is
//! nothing on the bus that reports which version of the daemon is answering or which
//! backends it was built with — both of which are the first two questions asked when a
//! scanner does not appear. This command reports them as empty rather than guessing:
//! reading them out of the exported objects would answer a different question ("which
//! backends found something"), and reading them off the filesystem would be this client
//! going around the API it exists to speak. [`scanbus-cli.md`] §11.4 records what
//! `Manager1` needs for these two rows to be filled in.
//!
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

use std::io::Write;

use scanbus_client::proxy::Manager1Proxy;
use scanbus_client::{BUS_NAME, Connection, Error as ClientError, Presence};

use crate::context::Context;
use crate::error::Result;
use crate::output::{self, Format};

/// Exit code for a daemon that is neither running nor startable ([`scanbus-cli.md`] §8).
///
/// [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md
const DAEMON_UNAVAILABLE: u8 = 3;

/// What the report holds, before it is a table or a JSON document.
struct Status {
    presence: Presence,
    owner: Option<String>,
    profiles: Vec<String>,
}

/// Reports, and answers 0 for a daemon that is there or could be, 3 for one that is not.
///
/// # Errors
///
/// [`Error`](crate::error::Error) when the *bus* cannot be reached or refuses — which is
/// a different failure from "no daemon" and must not be reported as one.
pub async fn run(context: &Context) -> Result<u8> {
    // `Bus::connect` rather than `Context::connect`: the latter honours `--no-activate`
    // by refusing when the name has no owner, which is exactly the case this command
    // exists to report. `status` is the command `--no-activate` cannot change the
    // outcome of, because it never activates in the first place.
    let connection = context
        .within(
            format!("connecting to the {} bus", context.bus),
            context.bus.connect(),
        )
        .await?;

    let status = collect(context, &connection).await?;
    report(context, &status)?;

    if status.presence.is_reachable() {
        return Ok(0);
    }

    // `absent` is a word, and the two facts behind it are what a user has to act on:
    // nobody is answering *and* nothing would answer. On stderr, in both formats, so
    // stdout stays exactly the report a script parses (§6).
    eprintln!(
        "scanbus: nothing owns {BUS_NAME} on the {} bus and it is not activatable there — \
         the daemon is not installed, or its D-Bus service file is not in place",
        context.bus
    );

    Ok(DAEMON_UNAVAILABLE)
}

/// Asks the bus, and then the daemon only if there is one.
async fn collect(context: &Context, connection: &Connection) -> Result<Status> {
    let presence = context
        .within(
            format!("asking the bus who owns {BUS_NAME}"),
            scanbus_client::presence(connection),
        )
        .await?;

    if presence != Presence::Running {
        // Activatable is deliberately not followed by a call: making one is how a health
        // check turns into a daemon launch, and the answer would be "running" for a
        // daemon this command started itself.
        return Ok(Status {
            presence,
            owner: None,
            profiles: Vec::new(),
        });
    }

    let owner = context
        .within(
            format!("reading the owner of {BUS_NAME}"),
            scanbus_client::owner(connection),
        )
        .await?;

    let profiles = context
        .within("reading the profile types", async {
            let manager = Manager1Proxy::new(connection).await?;
            Ok::<_, ClientError>(manager.get_profile_types().await?)
        })
        .await?;

    Ok(Status {
        presence,
        owner,
        profiles,
    })
}

/// Writes the report in whichever format was asked for.
fn report(context: &Context, status: &Status) -> Result<()> {
    let mut stdout = std::io::stdout().lock();

    match context.format {
        Format::Json => output::json(&mut stdout, &json(context, status)),
        Format::Human => human(&mut stdout, context, status),
    }
}

/// The human table: one row per fact, `-` for what this iteration cannot answer.
fn human(writer: &mut impl Write, context: &Context, status: &Status) -> Result<()> {
    let presence = status.presence.to_string();
    let presence = match status.presence {
        Presence::Running => context.style.good(&presence),
        Presence::Activatable => context.style.warn(&presence),
        Presence::Absent => context.style.bad(&presence),
    };

    output::fields(
        writer,
        context.style,
        &[
            ("daemon", presence),
            ("name", BUS_NAME.to_owned()),
            ("bus", context.bus.to_string()),
            ("owner", status.owner.clone().unwrap_or_default()),
            // Empty until Manager1 can be asked — see this module's header.
            ("version", String::new()),
            ("backends", String::new()),
            ("profiles", status.profiles.join(", ")),
            ("client", env!("CARGO_PKG_VERSION").to_owned()),
        ],
    )
}

/// The JSON document: `null` where the table prints `-`, so a `jq` filter can tell
/// "unknown" from "none".
fn json(context: &Context, status: &Status) -> serde_json::Value {
    serde_json::json!({
        "daemon": status.presence.to_string(),
        "name": BUS_NAME,
        "bus": context.bus.to_string(),
        "owner": status.owner,
        "version": serde_json::Value::Null,
        "backends": serde_json::Value::Null,
        "profiles": status.profiles,
        "client": env!("CARGO_PKG_VERSION"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    use crate::cli::Cli;

    fn context(args: &[&str]) -> Context {
        Context::new(&Cli::try_parse_from(args).unwrap().global)
    }

    fn running() -> Status {
        Status {
            presence: Presence::Running,
            owner: Some(":1.42".to_owned()),
            profiles: vec!["image".to_owned(), "document".to_owned()],
        }
    }

    fn absent() -> Status {
        Status {
            presence: Presence::Absent,
            owner: None,
            profiles: Vec::new(),
        }
    }

    #[test]
    fn the_table_reports_the_owner_and_the_profiles_of_a_running_daemon() {
        let mut out = Vec::new();
        human(&mut out, &context(&["scanbus", "status"]), &running()).unwrap();
        let out = String::from_utf8(out).unwrap();

        assert!(out.contains("daemon    running"), "{out}");
        assert!(out.contains("owner     :1.42"), "{out}");
        assert!(out.contains("profiles  image, document"), "{out}");
        // The two rows §11.4 is about, and the reason they are not simply omitted: a
        // missing row reads as "no backends", a `-` reads as "not answered".
        assert!(out.contains("version   -"), "{out}");
        assert!(out.contains("backends  -"), "{out}");
    }

    /// Nothing is invented for a daemon that is not there: no owner, no profiles.
    #[test]
    fn the_table_of_an_absent_daemon_holds_no_daemon_facts() {
        let mut out = Vec::new();
        human(&mut out, &context(&["scanbus", "status"]), &absent()).unwrap();
        let out = String::from_utf8(out).unwrap();

        assert!(out.contains("daemon    absent"), "{out}");
        assert!(out.contains("owner     -"), "{out}");
        assert!(out.contains("profiles  -"), "{out}");
    }

    /// `null`, not `""` or a missing key: a script asking whether the version is known
    /// gets an answer, and gets the same answer from every future version of this CLI.
    #[test]
    fn the_json_document_says_null_for_what_the_bus_cannot_answer() {
        let document = json(&context(&["scanbus", "--json", "status"]), &running());

        assert_eq!(document["daemon"], "running");
        assert_eq!(document["owner"], ":1.42");
        assert_eq!(document["profiles"][1], "document");
        assert!(document["version"].is_null());
        assert!(document["backends"].is_null());
        assert_eq!(document["name"], BUS_NAME);
    }

    /// `--bus` is echoed back, because a status report that does not say which bus it
    /// asked is unreadable on a machine with a private bus in the mix.
    #[test]
    fn the_report_names_the_bus_it_asked() {
        let context = context(&["scanbus", "--bus", "unix:path=/tmp/x", "--json", "status"]);
        assert_eq!(json(&context, &absent())["bus"], "unix:path=/tmp/x");
    }
}
