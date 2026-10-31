# ryo-v2 — pointers for coding agents

- What the project is and promises, and the crate layout: [README.md](README.md)
- Disclosure policy — read before anything ships:
  [PUBLIC_DEVELOPMENT.md](PUBLIC_DEVELOPMENT.md)
- Issue, branch, commit and PR conventions:
  [CONTRIBUTING.md](CONTRIBUTING.md)
- Security reports: [SECURITY.md](SECURITY.md)
- Decisions that outlive a change: the "Decisions in force" section of
  [SEARCH_LEDGER.md](SEARCH_LEDGER.md)
- API detail lives in RustDoc (`cargo doc`); code documentation outranks stale
  issue text.
- Never work on `main`; one branch per issue, cut from `origin/main` after a
  fetch. `just worktree-new <type> <slug>` from the main checkout does that and
  runs `just branch-check`.
- `just check` is green. [CONTRIBUTING.md](CONTRIBUTING.md#verification) says
  what it covers.
- Review a change locally before it goes up — by a person or an AI agent — and
  say in the PR body who or what reviewed it.
  [CONTRIBUTING.md](CONTRIBUTING.md#working-with-coding-agents--the-recommended-pattern)
  lists the questions a review answers.
- Development flow ([CONTRIBUTING.md](CONTRIBUTING.md#development-flow)): one worktree and
  branch per implementer; until the code base is functionally complete the maintainer merges into
  `main` locally.
- Open work lives in the ledgers ("Open work") or in the task's contract, not in an issue tracker.
- A decision code or data must keep gets a test or a ceiling in the same change.
- Files and commit messages never describe the execution environment; use placeholders such as
  `<BASE>` and `<OUT>`.
