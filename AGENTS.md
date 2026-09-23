# Working in this repo

## Verification
- Do not run broad workspace or whole-crate `cargo test` sweeps. If you think a test
  will genuinely help then just run or write tests that test behavior changed, filtered narrowly.
- Agents often leave tests that are useful for the PR, but have essentially no reusability.
  Before committing changes, make sure each test holds its place for the long term, delete any
  temporary tests you added.
- Right at the end before final changes e.g., before pushing the PR, 
  run `just fmt`, then `just clippy`, unless the user
  specifies otherwise.
  ```
  just fmt-check
  cargo clippy --all-features --no-deps -- -D warnings
  just test
  ```
  Use the plain `cargo clippy` command above, not `just clippy` — that
  recipe passes `--fix --allow-dirty` and rewrites files in place.

## Git and pull requests

- Do not commit directly on `develop` unless explicitly requested.
- When refactoring code, split the work into two commits: first move or
  reorganize code without logic changes, then make any logic changes in a
  separate commit.
- Create worktrees as siblings of the checkout. Use `{initials}/{topic}`
  branches and `<repo>-<branch-with-slashes-replaced-by-hyphens>` directories.
- Write PR descriptions for engineers without the code open: explain the
  problem, resulting behavior, necessary implementation details, and evidence
  for measurable claims. Do not include `Validation`/`Testing` sections or
  lists of commands run. Read [PR guidance](docs/pull-requests.md) before drafting
  or updating a description.

## Coding constraints
- This is a high performance project based around the core principles of the Flux library. 
  Do not add async code or runtimes to executables that do not already use async. 
  Always look for a solution using Flux tiles and Flux libraries first.
- Process items where they are produced; avoid transient collections built only
  to iterate immediately. Preserve the documented durable-state and borrow
  exceptions. Prefer stack storage and preallocated long-lived heap storage.
- Put type-specific behavior on the type. Extract a function only when it is
  longer than ten lines and called from more than one place.
- Never destructure `self` (`let Self { .. } = self`); use direct field
  access unless splitting borrows for the borrow checker.
- Comments explain hidden invariants and non-obvious reasons. Use them to
  clarify tricky code. Do not narrate code, edits, review fixes, or prior
  implementations.
- Always verify comments against the actual code. Build understanding by
  reading the implementation and tracing its control and data flow.