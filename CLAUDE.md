# Two documentation contexts

The docs are split by *when they are read*, not by subject. A task is either turning a
user story into an issue (**arch context**) or turning an issue into code (**dev
context**). Load one set, not both — that is the point of the split.

## Arch context — story → issue

What the system is and why. Read these when drafting or revising `docs/todo/<n>_<n>.md`.

| Doc | What it settles |
|---|---|
| `docs/scanbus-dbus-api.md` | The contract: object tree, the four interfaces, profiles, errors. The other arch docs are written against it. |
| `docs/scanbus-daemon-design.md` | Daemon side: the `ScannerBackend` trait, per-backend specifics, `PairingState`, the profile pipeline. |
| `docs/scanbus-cli.md` | The `scanbus` CLI — a client, and its §11 deltas the daemon owes it. |
| `docs/scanbus-gnome-gui.md` | The GTK4/libadwaita client, plus `docs/design/*.png`, the mockups it implements. |
| `docs/scanbus-mobile-backend.md` | The mobile backend, where the phone dials us. §10 is the list of things the Android app owes its own spec. |
| `docs/brother-skeyless-backend.md` | Brother with no vendor package. Supersedes the Brother half of the daemon design. |
| `docs/brother-brscan-arch.md` | Vendor background only — how Brother's stack works. Read it to justify a design, not to implement one. |
| `TODO.md` | Backlog of hardware and frontends not yet designed. Where a story comes from. |

## Dev context — issue → code

How the code is built. Read these when implementing an issue.

| Doc | What it settles |
|---|---|
| `README.md` | The authoritative crate table, the one-way dependency rule, MSRV, the `cargo`/`make` invocations. Start here. |
| `docs/scanbus-rust-implementation.md` | Workspace layout as planned, dependencies, development order, `.deb`/systemd/D-Bus packaging, testing strategy. |
| `CONTRIBUTING.md` | The manual GUI release checklist, run on real hardware. |

## Reading rules

- **The issue is the brief.** When implementing, the GitHub issue plus dev context is
  normally the whole of it.
- **Follow `Context:`, do not sweep.** An issue's `Context:` header names one arch doc
  and one section. Read that section when the code has to match a contract — not the
  whole doc, and not its siblings.
- `docs/scanbus-dbus-api.md` is the one arch doc regularly needed while coding, because
  it is the wire contract. Read the interface being touched, not all nine sections.
- **Do not read a doc "for background".** Every one of them is 200–600 lines of prose
  arguing a decision; they cost more context than they return unless the task turns on
  that decision.
- **When code and doc disagree, the code is what ships** — say so in the issue or fix
  the doc, do not silently follow either.

# Issue format

`submit_issue.sh` parses these files, so the header is structural, not decorative.
It takes the **title from the first line** (stripping `# `), reads the four `**Key:**`
lines, and uses **everything from the first `##` heading onward** as the issue body.

```markdown
# 3.1 — Repoint brscan-skey at our scripts, reversibly

**Workstream:** 3 — Scan actions
**Context:** [brother-skeyless-backend.md](../brother-skeyless-backend.md) §2 — repoint, don't replace
**Requires:** [1.2]
**State:** draft

## What

...

## Checklist

- [ ] ...

## Acceptance

- [ ] ...
```

Rules the script enforces, or that follow from how it works:

- **Filename is the issue number with dots as underscores** — `3_1.md` is issue 3.1.
- **The number must be in the title.** `Requires:` refs are resolved with
  `gh issue list --search "<ref> in:title"` to build `--blocked-by`, so a title without
  its number cannot be depended on.
- **`Requires:` refs are `[N.N]` in square brackets**, comma-separated or in prose;
  the extractor only sees the bracketed forms. Use `none — <why>` when there are no
  dependencies.
- **Submit in dependency order.** Refs resolve against issues that already exist on
  GitHub; an issue submitted before its dependency gets no `--blocked-by` link and the
  script says nothing about it.
- **`Workstream:` and `Context:` are re-emitted as `##` headings** at the top of the
  body. Do not repeat them in the prose.
- **`State:`** — `draft` until the issue is submitted.
- Requires `gh` and `gum`.

## Writing the body

Three sections, in this order:

- **What** — prose. What is being built, and *why it is this way rather than the
  obvious way*. Where the plan departs from `README.md` or from what the Brother
  package does, say so and give the reason; that argument is the reason the issue
  exists and is what stops the decision being silently reverted later. Name the
  concrete evidence — a config file's contents, a line from a stock script, a section
  of the arch doc named in `Context:`.
- **Checklist** — `- [ ]` items, the work itself. Specific enough to disagree with.
- **Acceptance** — `- [ ]` items, observable outcomes. What is run and what is seen,
  not "works correctly". Include the degraded and failure cases, not only the happy
  path.

**`Context:` must point at an arch doc and a section**, e.g.
`[scanbus-dbus-api.md](../scanbus-dbus-api.md) §3`. It is what lets the implementer
read one section instead of the set, so a link to a whole document is a defect in the
issue. Cross-reference other issues as `[3.4](3_4.md)`.

# Planning a task, then filing it

Arch context. Two steps, deliberately separate:

1. **Draft.** Write `docs/todo/<number with underscores>.md`. The format is what
   [scripts/submit_issue.sh](scripts/submit_issue.sh) parses, so it is not free
   text: an `# <number> — <title>` first line, then `**Workstream:**`,
   `**Context:**` (a link into `docs/`), `**Requires:**` (task numbers in
   brackets, `[4.3]`), `**State:**`, then the `## What` / `## Checklist` /
   `## Acceptance` body. Everything from the first `## ` on becomes the issue
   body. Iterate on the draft as much as needed — nothing has been filed yet.
2. **Submit.** `scripts/submit_issue.sh docs/todo/6_1.md` previews it, asks for
   confirmation, and creates the issue. It resolves each `Requires:` number to
   an existing issue by title search and passes them as `--blocked-by`, so
   **file dependencies before what depends on them** or the link is silently
   dropped. Then `git rm` the draft — the issue is the record now.

Needs `gh` (authenticated) and `gum`.

If drafting reveals the arch doc is wrong or silent, fix the doc in the same change —
an issue that carries design nobody can find later is how the split stops working.

# Working an issue

Dev context. Ask the user whether to work on the main checkout or a dedicated branch
*and* a git worktree for it. If dedicated branch:

```sh
git worktree add .worktrees/<issue>-<topic> -b <issue>-<topic> origin/master
```

The branch name starts with the **issue number**, then a short kebab-case name
for the work: `117-fix-wifi-scanning`, `104-network-step`. Do the whole task in
that worktree — it keeps the main checkout free for parallel work
and shares one object store, so no re-clone. Remove it with
`git worktree remove` once the branch is merged.

Let user run build, test, fmt and lint manually.
