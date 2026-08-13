# Documentation layout

- `README.md` — what the extension is for.
- `docs/*scanbus-cli*.md` - specifications
- `docs/todo/<n>_<n>.md` — one file per issue, submitted to GitHub with
  `./scripts/submit_issue.sh docs/todo/3_1.md`.

# Issue format

`submit_issue.sh` parses these files, so the header is structural, not decorative.
It takes the **title from the first line** (stripping `# `), reads the four `**Key:**`
lines, and uses **everything from the first `##` heading onward** as the issue body.

```markdown
# 3.1 — Repoint brscan-skey at our scripts, reversibly

**Workstream:** 3 — Scan actions
**Context:** [design.md](../design.md) §2.1 — repoint, don't replace
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
  concrete evidence — a config file's contents, a line from a stock script, a fact
  from `docs/design.md` §1.
- **Checklist** — `- [ ]` items, the work itself. Specific enough to disagree with.
- **Acceptance** — `- [ ]` items, observable outcomes. What is run and what is seen,
  not "works correctly". Include the degraded and failure cases, not only the happy
  path.

Cross-reference other issues as `[3.4](3_4.md)` and design sections as
`[design.md](../design.md) §2.1`.

# Planning a task, then filing it

Two steps, deliberately separate:

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

# Working an issue

When working an issue, ask user whether to work on main checkout or a dedicated
branch *and* a git worktree for it. If dedicated branch :

```sh
git worktree add .worktrees/<issue>-<topic> -b <issue>-<topic> origin/master
```

The branch name starts with the **issue number**, then a short kebab-case name
for the work: `117-fix-wifi-scanning`, `104-network-step`. Do the whole task in
that worktree — it keeps the main checkout free for parallel work
and shares one object store, so no re-clone. Remove it with
`git worktree remove` once the branch is merged.

Let user run test, fmt and lint manually.