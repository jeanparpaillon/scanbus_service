//! `scanbus manpage` — generate `scanbus(1)` and the page of every subcommand locally.
//!
//! # Why a directory, and not just stdout
//!
//! `clap_mangen` renders the SUBCOMMANDS section of `scanbus(1)` as a list of *man
//! references* — `scanbus-scan(1)`, `scanbus-profile(1)`, one per subcommand — and `man`
//! resolves those against installed files. Shipping the top-level page alone therefore
//! documents fourteen commands by pointing at fourteen pages that do not exist, and
//! `man scanbus-scan` answers "No manual entry". A single page also cannot carry them:
//! each subcommand needs its own `.TH`, so the tree is a tree of files, and stdout can
//! hold exactly one of them. `--output-dir` is what writes the rest; bare `scanbus
//! manpage` keeps printing `scanbus(1)` so that reading one page still needs no
//! temporary directory.
//!
//! The tree is walked from the same `clap` command the parser is built from, so a
//! subcommand added to [`Cli`] gets a page with no change here — which is the property
//! that stops the two drifting apart again.
//!
//! # SEE ALSO is ours
//!
//! `clap_mangen` emits no `SEE ALSO`, so the references only ever point downwards: from
//! `scanbus(1)` into the subcommands. A reader who arrives at `scanbus-profile-list(1)`
//! from `apropos` has nothing to climb back up, so the section is appended here, naming
//! the root and — for a nested command — its parent.

use std::io::Write as _;
use std::path::Path;

use clap::CommandFactory as _;
use clap_mangen::Man;

use crate::cli::Cli;
use crate::error::{Error, Result};

/// Writes `scanbus(1)` to stdout, or the whole page tree into `output_dir`.
pub fn run(output_dir: Option<&Path>) -> Result<u8> {
    let command = root();

    match output_dir {
        None => {
            let mut stdout = std::io::stdout().lock();
            let page = render(command, &[])?;
            stdout.write_all(&page).map_err(Error::write)?;
        }
        Some(directory) => {
            std::fs::create_dir_all(directory)
                .map_err(|error| Error::write(annotate(directory, &error)))?;
            generate(&command, &[], directory)?;
        }
    }

    Ok(0)
}

/// The command as the pages describe it.
///
/// `disable_help_subcommand` even for the single page on stdout: `clap`'s generated
/// `help` subcommand would otherwise be listed there as `scanbus-help(1)`, a reference
/// to a page that no `--output-dir` run writes and that no reader wants. `build()` is
/// what gives each subcommand the `scanbus-profile-list` display name that the
/// filenames, the `.TH` lines and the parent's cross-references are all taken from.
fn root() -> clap::Command {
    let mut command = Cli::command().disable_help_subcommand(true);
    command.build();
    command
}

/// Writes the page for `command`, then one for each of its subcommands.
///
/// `ancestors` is the chain from the root down to `command`'s parent, in display-name
/// form (`["scanbus", "scanbus-profile"]`), which is what `SEE ALSO` is built from.
fn generate(command: &clap::Command, ancestors: &[String], directory: &Path) -> Result<()> {
    let page = render(command.clone(), ancestors)?;
    let path = directory.join(filename(command));
    std::fs::write(&path, page).map_err(|error| Error::write(annotate(&path, &error)))?;

    let mut below = ancestors.to_vec();
    below.push(display_name(command));
    for subcommand in command.get_subcommands().filter(|sub| !sub.is_hide_set()) {
        generate(subcommand, &below, directory)?;
    }

    Ok(())
}

/// One page, as roff, with the `SEE ALSO` `clap_mangen` does not write.
fn render(command: clap::Command, ancestors: &[String]) -> Result<Vec<u8>> {
    let mut page = Vec::new();
    // The footer's centre line: `clap_mangen` defaults it to the command's own version,
    // which only the root has — a subcommand would be footed with its bare name, so
    // `scanbus-profile-list(1)` would print "list" where every other page prints the
    // release it belongs to.
    Man::new(command)
        .source(format!("scanbus {}", env!("CARGO_PKG_VERSION")))
        .render(&mut page)
        .map_err(Error::write)?;

    if let Some(root) = ancestors.first() {
        page.extend_from_slice(b".SH SEE ALSO\n");
        // The immediate parent, then the root — skipping the parent when it *is* the
        // root, which is every one-level command.
        let mut referenced: Vec<&String> = ancestors.iter().rev().take(1).collect();
        if referenced.first().copied() != Some(root) {
            referenced.push(root);
        }
        let references: Vec<String> = referenced
            .iter()
            .map(|name| format!("\\fB{}\\fR(1)", name.replace('-', "\\-")))
            .collect();
        page.extend_from_slice(references.join(", ").as_bytes());
        page.push(b'\n');
    }

    Ok(page)
}

/// `scanbus-profile-list.1`, matching the reference the parent page prints.
fn filename(command: &clap::Command) -> String {
    format!("{}.1", display_name(command))
}

/// The dashed name `clap` derived for the command: `scanbus`, `scanbus-profile-list`.
fn display_name(command: &clap::Command) -> String {
    command
        .get_display_name()
        .unwrap_or_else(|| command.get_name())
        .to_owned()
}

/// The path in front of the I/O error, since `std::io::Error` never carries one and
/// "No such file or directory" on its own does not say which directory.
fn annotate(path: &Path, error: &std::io::Error) -> std::io::Error {
    std::io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The name a page is filed under is the name the parent's SUBCOMMANDS section
    /// prints, at every depth — the dead cross-reference is exactly what a mismatch is.
    #[test]
    fn nested_pages_are_named_after_the_full_command() {
        let command = root();

        let profile = command
            .get_subcommands()
            .find(|sub| sub.get_name() == "profile")
            .expect("`profile` is a subcommand");
        let list = profile
            .get_subcommands()
            .find(|sub| sub.get_name() == "list")
            .expect("`profile list` is a subcommand");

        assert_eq!(filename(&command), "scanbus.1");
        assert_eq!(filename(profile), "scanbus-profile.1");
        assert_eq!(filename(list), "scanbus-profile-list.1");
    }

    /// A leaf points at its parent and at the root; a one-level command names the root
    /// once rather than twice; the root itself has no `SEE ALSO` to write.
    #[test]
    fn see_also_climbs_back_up() {
        let root = String::from("scanbus");
        let profile = String::from("scanbus-profile");

        let page = String::from_utf8(
            render(
                clap::Command::new("list").about("x"),
                &[root.clone(), profile],
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            page.contains("\\fBscanbus\\-profile\\fR(1), \\fBscanbus\\fR(1)"),
            "{page}"
        );

        let page =
            String::from_utf8(render(clap::Command::new("status").about("x"), &[root]).unwrap())
                .unwrap();
        assert!(
            page.contains(".SH SEE ALSO\n\\fBscanbus\\fR(1)\n"),
            "{page}"
        );

        let page =
            String::from_utf8(render(clap::Command::new("scanbus").about("x"), &[]).unwrap())
                .unwrap();
        assert!(!page.contains("SEE ALSO"), "{page}");
    }
}
