//! The command surface of [`scanbus-cli.md`] §3, as `clap` sees it.
//!
//! The whole surface is declared here, in one file, even though most of it is still a
//! stub ([`crate::cmd`]): `--help` is the contract a user reads first, and a CLI that
//! grows its help text one issue at a time never has a moment where the help and the
//! design document agree. What a command *does* arrives per issue; what it *accepts*
//! is fixed now and reviewed against §3 once.
//!
//! # Global options are `global = true`, one by one
//!
//! `clap` puts a flattened struct's arguments on the top-level command only, so
//! `scanbus --json status` would parse and `scanbus status --json` would not — the
//! second being the one everybody types. `global = true` on each argument propagates it
//! to every subcommand, which is what makes the two spellings the same command.
//!
//! # `--id` is not one of them
//!
//! §3's table of global options does not list `--id`, and it is not made one here even
//! though every command that takes a scanner accepts it. A truly global flag would appear
//! in the help of `status` and `profile list`, which take no selector and would silently
//! ignore it. It rides on [`ScannerArg`] instead — flattened into each command that names
//! a scanner — so the help offers it where it means something and nowhere else.
//!
//! [`scanbus-cli.md`]: https://github.com/jeanparpaillon/scanbus_service/blob/master/docs/scanbus-cli.md

use std::time::Duration;

use clap::{ArgGroup, Args, Parser, Subcommand};
use scanbus_client::{Bus, Match};

use crate::duration::parse_duration;

/// `scanbus [GLOBAL] <command> [ARGS]`.
#[derive(Debug, Parser)]
#[command(
    name = "scanbus",
    version,
    about = "Command-line client for the scanbus D-Bus service",
    long_about = "Command-line client for the scanbus D-Bus service.\n\n\
                  Speaks org.scanbus on the session bus and nothing else: it never opens \
                  a scanner and never touches a backend's configuration itself.",
    // No subcommand is a usage error, not a silent success — and `clap` exits 2 for it,
    // which is the code §8 gives usage errors.
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(flatten)]
    pub global: Global,

    #[command(subcommand)]
    pub command: Command,
}

/// The global options of §3, valid before or after the subcommand.
#[derive(Debug, Args)]
pub struct Global {
    /// Machine-readable output; implies --no-color
    #[arg(long, global = true)]
    pub json: bool,

    /// Never colourise; also honours NO_COLOR and a non-terminal stdout
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Client-side logging to stderr; -vv for debug. Does NOT change the daemon's log
    /// level — use `journalctl --user -u scanbus` for that
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Ceiling on each D-Bus call, e.g. 500ms, 30s, 2m
    #[arg(
        long,
        global = true,
        value_name = "DURATION",
        default_value = "30s",
        value_parser = parse_duration
    )]
    pub timeout: Duration,

    /// Bus to talk to: session, system, or a raw address
    #[arg(long, global = true, value_name = "BUS", default_value = "session")]
    pub bus: Bus,

    /// Fail instead of letting a call D-Bus-activate the daemon
    #[arg(long, global = true)]
    pub no_activate: bool,
}

/// A scanner, and the flag that pins how it is read (§5).
///
/// Flattened into every command that names one, so that the four spellings and the one
/// way to refuse three of them are declared once. `--id` is what a script uses: a `Name`
/// is a human label the device is free to change, an `Id` is contractual ([1.2]), and a
/// script matching on a name works until the day it silently selects a different scanner.
///
/// [1.2]: https://github.com/jeanparpaillon/scanbus_service/issues/2
#[derive(Debug, Args)]
pub struct ScannerArg {
    /// Object path, exact id, unique id prefix, or unique substring of a name
    pub scanner: String,

    /// Read SCANNER as an exact Id: no path, no prefix, no name. What scripts should use
    #[arg(long)]
    pub id: bool,
}

impl ScannerArg {
    /// How §5's ladder is to be climbed for this argument.
    pub const fn matching(&self) -> Match {
        if self.id { Match::ExactId } else { Match::Any }
    }
}

/// The optional `--scanner` of the `job` listings, with the same `--id`.
///
/// A separate type from [`ScannerArg`] because the selector is optional here — `job list`
/// with no scanner is every job — and `--id` without it would be a flag qualifying
/// nothing, which `requires` turns into a usage error rather than a silent no-op.
#[derive(Debug, Args)]
pub struct ScannerFilter {
    /// Only jobs from this scanner
    #[arg(long, value_name = "SCANNER")]
    pub scanner: Option<String>,

    /// Read --scanner as an exact Id: no path, no prefix, no name
    #[arg(long, requires = "scanner")]
    pub id: bool,
}

impl ScannerFilter {
    /// How §5's ladder is to be climbed for `--scanner`, when there is one.
    pub const fn matching(&self) -> Match {
        if self.id { Match::ExactId } else { Match::Any }
    }
}

/// The commands of §3.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Daemon presence, owner, version, backends and profile types
    ///
    /// The only command that tolerates an absent daemon, and the only one that never
    /// activates it: it asks the bus who owns the name rather than calling the daemon,
    /// so it is usable in a health check that must have no side effects. Exits 0 for a
    /// daemon that is running or activatable, 3 for one that is neither.
    ///
    /// Version and backends are reported as `-`: org.scanbus.Manager1 has no property
    /// that carries either, so there is nothing to read. See scanbus-cli.md §11.
    Status,

    /// Scanners the daemon knows about right now
    ///
    /// Never starts discovery — a freshly started daemon that has restored two paired
    /// scanners lists two scanners. `scanbus discover` is what goes looking.
    List {
        /// Only scanners paired with this host
        #[arg(long, conflicts_with = "unpaired")]
        paired: bool,

        /// Only scanners that are not paired
        #[arg(long)]
        unpaired: bool,
    },

    /// Every property of one scanner, plus its buttons
    Show {
        #[command(flatten)]
        scanner: ScannerArg,
    },

    /// Start a discovery session and stream what appears
    ///
    /// Example: scanbus discover --backend sane,avahi --for 30s
    ///
    /// The session ends when the command does, and the unpaired scanners it printed
    /// cease to exist with it; --keep leaves it running for the next command.
    Discover {
        /// Restrict discovery to these backends
        #[arg(long = "backend", value_name = "ID", value_delimiter = ',')]
        backends: Vec<String>,

        /// How long to hold the discovery session
        #[arg(
            long = "for",
            value_name = "DURATION",
            default_value = "10s",
            value_parser = parse_duration
        )]
        duration: Duration,

        /// Run until interrupted, ignoring --for
        #[arg(long, conflicts_with = "duration")]
        watch: bool,

        /// Leave discovery running after this command exits
        #[arg(long)]
        keep: bool,
    },

    /// Pair a scanner with this host
    ///
    /// Example: scanbus pair MFC-L2710DW --no-wait
    ///
    /// Waits for PairingState to reach done or failed by default, because the alternative
    /// is a script that polls. --no-wait returns as soon as the method call does, which is
    /// what the D-Bus API itself does. Interrupting a wait does not cancel the pairing:
    /// the daemon owns it, and `scanbus cancel-pairing` is what stops it.
    Pair {
        #[command(flatten)]
        scanner: ScannerArg,

        /// Return as soon as Pair() returns, without waiting for the verdict
        #[arg(long)]
        no_wait: bool,
    },

    /// Cancel a pairing in progress
    CancelPairing {
        #[command(flatten)]
        scanner: ScannerArg,
    },

    /// Remove a pairing, and the persisted state that goes with it
    Unpair {
        #[command(flatten)]
        scanner: ScannerArg,

        /// Do not prompt for confirmation
        #[arg(long)]
        yes: bool,
    },

    /// Declare this host ready to receive from a scanner
    Connect {
        #[command(flatten)]
        scanner: ScannerArg,

        /// Session profile, one of the values `scanbus profile list` reports
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
    },

    /// Stop listening for a scanner's events
    Disconnect {
        #[command(flatten)]
        scanner: ScannerArg,
    },

    /// Host-driven scan
    ///
    /// Optional in the D-Bus API: a daemon that does not implement Scan() answers
    /// org.freedesktop.DBus.Error.UnknownMethod, and this command says so rather than
    /// failing obscurely.
    Scan {
        #[command(flatten)]
        scanner: ScannerArg,

        /// Profile to apply to this scan
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,

        /// Profile option, repeatable
        #[arg(long = "option", value_name = "K=V")]
        options: Vec<String>,

        /// Print the job id and exit instead of following the job
        #[arg(long)]
        no_wait: bool,
    },

    /// The scanner's physical keys, and what this host runs for them
    Button {
        #[command(subcommand)]
        command: ButtonCommand,
    },

    /// Scans in flight
    Job {
        #[command(subcommand)]
        command: JobCommand,
    },

    /// Profiles this daemon runs, and their options
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },

    /// Raw signal firehose under /org/scanbus, for debugging
    Monitor {
        /// Only signals from objects under this path
        #[arg(long, value_name = "PREFIX")]
        path: Option<String>,
    },
}

/// `scanbus button …` — §3, and §5 of the D-Bus API for what is read-only.
#[derive(Debug, Subcommand)]
pub enum ButtonCommand {
    /// The physical menu, with what each entry is assigned
    List {
        #[command(flatten)]
        scanner: ScannerArg,
    },

    /// Assign a profile, a label or options to one key
    ///
    /// Example: scanbus button set MFC 2 --profile document --option dir=~/Documents
    ///
    /// The engraved label is the firmware's and is read-only on most devices; --label
    /// only works where the device says it is configurable.
    #[command(group(
        ArgGroup::new("change")
            .required(true)
            .multiple(true)
            .args(["profile", "label", "options", "option_json"])
    ))]
    Set {
        #[command(flatten)]
        scanner: ScannerArg,

        /// Key index, or a unique substring of its device label
        button: String,

        /// Profile to run when this key is pressed
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,

        /// Label to display on the device, where the device allows it
        #[arg(long, value_name = "TEXT")]
        label: Option<String>,

        /// Profile option for this key, repeatable
        #[arg(long = "option", value_name = "K=V")]
        options: Vec<String>,

        /// Profile option with the value parsed as JSON, repeatable
        #[arg(long = "option-json", value_name = "K=JSON")]
        option_json: Vec<String>,
    },

    /// Unassign a key
    Clear {
        #[command(flatten)]
        scanner: ScannerArg,

        /// Key index, or a unique substring of its device label
        button: String,
    },
}

/// `scanbus job …` — §3.
#[derive(Debug, Subcommand)]
pub enum JobCommand {
    /// Jobs the daemon is holding right now
    List {
        #[command(flatten)]
        filter: ScannerFilter,
    },

    /// Everything about one job
    Show {
        /// Short job id, or a full object path
        job: String,
    },

    /// Follow jobs as they progress
    Watch {
        #[command(flatten)]
        filter: ScannerFilter,

        /// Exit when the first job followed reaches a terminal state
        #[arg(long)]
        until_done: bool,
    },
}

/// `scanbus profile …` — §3.
#[derive(Debug, Subcommand)]
pub enum ProfileCommand {
    /// Profile types this daemon will run
    List,

    /// One profile's options
    Show {
        /// Profile name, e.g. document
        name: String,
    },

    /// Set options on a profile
    Set {
        /// Profile name, e.g. document
        name: String,

        /// Option assignments
        #[arg(value_name = "K=V", required = true)]
        options: Vec<String>,

        /// Option values written as JSON
        #[arg(long = "option-json", value_name = "K=JSON")]
        option_json: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    /// `clap`'s own audit of the derive: conflicting ids, an argument declared twice, a
    /// `global` on something that cannot carry it. It panics on a fault, and a CLI whose
    /// definition is this much declaration is exactly what it is for.
    #[test]
    fn the_command_definition_is_well_formed() {
        Cli::command().debug_assert();
    }

    /// The point of `global = true`: both orders are the same command.
    #[test]
    fn a_global_option_may_come_before_or_after_the_subcommand() {
        let before = Cli::try_parse_from(["scanbus", "--json", "status"]).unwrap();
        let after = Cli::try_parse_from(["scanbus", "status", "--json"]).unwrap();

        assert!(before.global.json && after.global.json);
        assert!(matches!(before.command, Command::Status));
        assert!(matches!(after.command, Command::Status));
    }

    /// The defaults of §3's table, which no other test would notice going wrong.
    #[test]
    fn the_defaults_are_the_ones_the_design_documents() {
        let cli = Cli::try_parse_from(["scanbus", "status"]).unwrap();

        assert_eq!(cli.global.timeout, Duration::from_secs(30));
        assert_eq!(cli.global.bus, Bus::Session);
        assert!(!cli.global.json);
        assert!(!cli.global.no_color);
        assert!(!cli.global.no_activate);
        assert_eq!(cli.global.verbose, 0);
    }

    /// `--bus` takes an address, which is how the test harness reaches a private bus.
    #[test]
    fn the_bus_option_takes_an_address_as_well_as_a_name() {
        let cli = Cli::try_parse_from(["scanbus", "--bus", "unix:path=/tmp/x", "status"]).unwrap();
        assert_eq!(cli.global.bus, Bus::Address("unix:path=/tmp/x".to_owned()));
    }

    /// No subcommand is a usage error (exit 2), not an empty success.
    #[test]
    fn no_arguments_is_a_usage_error() {
        let error = Cli::try_parse_from(["scanbus"]).expect_err("a bare invocation is not a run");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        assert_eq!(error.exit_code(), 2, "§8 gives usage errors exit 2");
    }

    /// `-v` counts, because the design has two levels and no more.
    #[test]
    fn verbosity_counts_up() {
        for (args, expected) in [
            (vec!["scanbus", "status"], 0),
            (vec!["scanbus", "-v", "status"], 1),
            (vec!["scanbus", "-vv", "status"], 2),
        ] {
            assert_eq!(Cli::try_parse_from(args).unwrap().global.verbose, expected);
        }
    }

    /// The comma-separated list of §3, and the repeatable option flags of `button set`.
    #[test]
    fn the_repeatable_options_parse_the_way_the_help_says() {
        let cli = Cli::try_parse_from(["scanbus", "discover", "--backend", "sane,avahi"]).unwrap();
        let Command::Discover { backends, .. } = cli.command else {
            panic!("that is a discover")
        };
        assert_eq!(backends, ["sane", "avahi"]);

        let cli = Cli::try_parse_from([
            "scanbus", "button", "set", "MFC", "2", "--option", "dir=/tmp", "--option", "dpi=300",
        ])
        .unwrap();
        let Command::Button {
            command:
                ButtonCommand::Set {
                    options,
                    option_json,
                    ..
                },
        } = cli.command
        else {
            panic!("that is a button set")
        };
        assert_eq!(options, ["dir=/tmp", "dpi=300"]);
        assert!(option_json.is_empty());

        let cli = Cli::try_parse_from([
            "scanbus",
            "button",
            "set",
            "MFC",
            "2",
            "--option-json",
            "profile={\"dir\":\"/tmp\"}",
        ])
        .unwrap();
        let Command::Button {
            command: ButtonCommand::Set { option_json, .. },
        } = cli.command
        else {
            panic!("that is a button set")
        };
        assert_eq!(option_json, ["profile={\"dir\":\"/tmp\"}"]);

        let cli = Cli::try_parse_from([
            "scanbus",
            "profile",
            "set",
            "image",
            "quality=85",
            "--option-json",
            "output_folder=\"/tmp/scans\"",
        ])
        .unwrap();
        let Command::Profile {
            command:
                ProfileCommand::Set {
                    options,
                    option_json,
                    ..
                },
        } = cli.command
        else {
            panic!("that is a profile set")
        };
        assert_eq!(options, ["quality=85"]);
        assert_eq!(option_json, ["output_folder=\"/tmp/scans\""]);
    }

    /// `--paired --unpaired` asks for the empty set; `--watch --for` asks for two
    /// different session lengths. Both are caught by `clap` rather than by the daemon.
    #[test]
    fn the_contradictory_flag_pairs_are_refused() {
        assert!(Cli::try_parse_from(["scanbus", "list", "--paired", "--unpaired"]).is_err());
        assert!(Cli::try_parse_from(["scanbus", "discover", "--watch", "--for", "5s"]).is_err());
    }

    #[test]
    fn button_set_requires_at_least_one_change() {
        assert!(Cli::try_parse_from(["scanbus", "button", "set", "MFC", "2"]).is_err());
        assert!(
            Cli::try_parse_from([
                "scanbus",
                "button",
                "set",
                "MFC",
                "2",
                "--profile",
                "document"
            ])
            .is_ok()
        );
    }
}
