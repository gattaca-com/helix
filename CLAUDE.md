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

- Write code, not prose. I write the issues, the PR descriptions, the
  commit messages, and the code comments. Don't draft them, and don't
  put specs or plans in `docs/`. Report your findings to me in chat
  instead.
- Tests define correctness. When starting a step with no tests yet,
  write the tests first. Get explicit human approval before writing
  implementation code — use plan mode, or ask directly and wait for an
  answer.
- Scope each change to one step of one issue.
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
