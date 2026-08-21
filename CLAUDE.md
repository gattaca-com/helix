# Working in this repo

## Communication

- Report to me using ASD-STE100 Simplified Technical English.
- Be concise and direct.
- Use short sentences and the active voice.
- Give one instruction in each sentence.
- Use the same term for the same item.
- Do not change commands, code, identifiers, or quotations to meet these language rules.

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the full workflow. Operational
rules for you specifically:

- Tests define correctness. When starting a step with no tests yet, draft
  the tests (and the issue, if it needs updating) first. Get explicit
  human approval before writing implementation code — use plan mode, or
  ask directly and wait for an answer.
- If a nontrivial change has no GitHub issue yet, draft one using the
  matching template under
  [`.github/ISSUE_TEMPLATE/`](.github/ISSUE_TEMPLATE/) (`feature`, `bug`,
  `refactor`, `perf`) for review. Don't put specs or plans in `docs/`.
- Scope each PR to one step of one issue. Use
  [`.github/PULL_REQUEST_TEMPLATE.md`](.github/PULL_REQUEST_TEMPLATE.md),
  including a "what this deliberately does not do" note when scope could
  be mistaken for an oversight.
- Before declaring a step done, run:

  ```
  just fmt-check
  cargo clippy --all-features --no-deps -- -D warnings
  just test
  ```

  Use the plain `cargo clippy` command above, not `just clippy` — that
  recipe passes `--fix --allow-dirty` and rewrites files in place. `just
  test` requires a local Postgres (`just local-postgres`) for
  `helix-database`'s tests.
