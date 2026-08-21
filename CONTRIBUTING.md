# Contributing to helix

## Workflow

Before begining work on an issues check to ensure no one else has
already picked it up. If they have an it's a large issue with many
steps, divide the steps between you.

If no issue exists which captures the change you had in mind create one
if needed (see issues below).

Design, review, and testing are human responsibilities. Implementation
work can be delegated to an agent to speed things up. But we follow
a trust but verify approach for ai work.

Tests define correctness, not issues or design docs. Write tests before
or alongside implementation, and run them throughout, not just at the end.
A human must review and approve the tests (and the issue, if there is one)
before implementation code is written against them.

## Issues

A trivial one-liner (typo, obvious small fix) can go straight to a small
PR — no issue required. Any actual behavior or feature change gets a
GitHub issue first, using the matching template under
[`.github/ISSUE_TEMPLATE/`](.github/ISSUE_TEMPLATE/) (`feature`, `bug`,
`refactor`, `perf`).

Large issues get a step checklist, one PR per step. The checklist can grow
as work proceeds.

## Pull requests

Scope each PR to one step of one issue. Use
[`.github/PULL_REQUEST_TEMPLATE.md`](.github/PULL_REQUEST_TEMPLATE.md).

## Verification bar

Before merging:

```
just fmt-check
cargo clippy --all-features --no-deps -- -D warnings
just test
```

`just clippy` runs `clippy --fix --allow-dirty` and rewrites files in
place — use the plain `cargo clippy ... -D warnings` command above for
checking; use `just clippy` only when fixes should be applied.

`just test` requires a local Postgres for `helix-database`'s tests: run
`just local-postgres` first.

## Review checklist

- CI (`lint`, `unit-test`) is green.
- The diff matches the linked issue/step, and the tests exercise the
  claimed behavior.
- No unexplained scope creep, unrelated files, or security-sensitive
  changes (auth, signature verification, payment paths) without extra
  scrutiny.

## Branches

Personal-prefixed branch names (`od/...`-style). PRs squash-merge into
`develop`, not `main`.
