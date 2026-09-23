# Agent conventions

Read this before writing any code. [HANDOFF.md](HANDOFF.md) has the
environment/workflow details (worktrees, screenshots, CRLF, etc.); [PLAN.md](PLAN.md)
has the architecture and open-work list. This file is the quality bar.

## Quality bar

- **Clean and readable**: match the existing module's style and naming rather
  than inventing a parallel convention. Prefer clarity over cleverness. No
  dead code, no commented-out code, no unexplained abbreviations.
- **Performant**: avoid needless allocation, cloning, or re-computation in
  hot paths (render, IMAP fetch, search). If a change trades performance for
  simplicity, say so in the PR description.
- **Quality UX**: a feature is not done until it behaves correctly from the
  user's side, not just until it compiles. Error states get a visible banner
  or message, not a silent failure. Follow HANDOFF.md's "verify visual work
  by looking at it" for anything touching rendering, layout, or the webview.
- **Module size**: keep modules at or under **500 lines**. If a change would
  push a file over that, split it along an existing seam (e.g. a submodule)
  rather than letting it grow — and say so in the PR if the split changes
  public structure.
- **Comments**: only where the *why* is non-obvious (a workaround, an
  invariant, a subtle protocol detail). Do not narrate *what* the code does.

## Before opening a PR

All of these must pass locally — do not open a PR on red:

```bash
cargo build --workspace
cargo test --workspace
```

If your change affects the IMAP/SMTP simulation suite, also run
`cargo test --workspace -- --include-ignored` with `ESMAIL_TEST_CA_TRUSTED=1`
set (see `crates/mail-mock-server/README.md`) when you can; if you can't set
up the test CA in your environment, say so explicitly in the PR rather than
silently skipping it.

CI runs the same build + test commands on Linux and Windows — a PR that
fails CI is not finished work.

## Scope discipline

Touch only the files your issue names. If finishing correctly requires
touching a file another in-flight issue owns, say so in the PR description
instead of guessing around it.
