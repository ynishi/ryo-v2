# ryo-v2 — pointers for coding agents

- What the project is and promises, and the crate layout: [README.md](README.md)
- The concept every design decision rests on — a change is checked against it:
  [CONCEPT.md](CONCEPT.md)
- Disclosure policy — read before anything ships:
  [PUBLIC_DEVELOPMENT.md](PUBLIC_DEVELOPMENT.md)
- Issue, branch, commit and PR conventions:
  [CONTRIBUTING.md](CONTRIBUTING.md)
- Security reports: [SECURITY.md](SECURITY.md)
- How decisions are made and recorded, and how contracts change:
  [DEVELOPMENT.md](DEVELOPMENT.md#decisions-and-contracts)
- API detail lives in RustDoc (`cargo doc`); code documentation outranks stale
  issue text.
- Never work on `main`; one branch per issue, cut from `origin/main` after a
  fetch. `just worktree-new <type> <slug>` from the main checkout does that and
  runs `just branch-check`.
- `just check` is green. [CONTRIBUTING.md](CONTRIBUTING.md#verification) says
  what it covers; it refuses a branch that does not contain `origin/main` (the
  local `main` where there is none), so merge it into the branch before the
  final check.
- `just ledger-merge-install`, once per checkout, makes the ledgers merge by
  section and table row ([CONTRIBUTING.md](CONTRIBUTING.md#branches)).
- Review a change locally before it goes up — by a person or an AI agent — and
  record who or what reviewed it: a GitHub review or the PR body, or, with a
  local merge, the hand-over note and the merge commit's `Reviewed:` line.
  [DEVELOPMENT.md](DEVELOPMENT.md#working-with-coding-agents--the-recommended-pattern)
  lists the questions a review answers.
- Development flow ([DEVELOPMENT.md](DEVELOPMENT.md#development-flow)): one worktree and
  branch per implementer; until the code base is functionally complete the maintainer merges into
  `main` locally.
- In that phase there is no issue tracker and no list of work items: work is handed over in the
  task's contract, and the ledgers hold only what was decided and measured.
- A decision code or data must keep gets a test or a ceiling in the same change.
- Files and commit messages never describe the execution environment (host, account, path); use
  placeholders such as `<BASE>` and `<OUT>`. Core count and memory beside a measurement stay.
