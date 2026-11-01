# Development

How development is done in this repository: how decisions are made and recorded, how contracts
change, how documentation is written, and how work flows to `main`. The concept every decision
rests on is [CONCEPT.md](CONCEPT.md); the basic conventions (issues, branches, commits, pull
requests) are in [CONTRIBUTING.md](CONTRIBUTING.md). The disclosure policy in
[PUBLIC_DEVELOPMENT.md](PUBLIC_DEVELOPMENT.md) outranks both.

Three kinds of statement share these files, and the boundary between them is a rule of its own.
What the repository decides binds everyone who contributes: never on `main`, the commit format,
what a gate is and when it runs. What installed tooling decides travels with the tool: a plugin's
agents behave the same in every repository that installs them, and these files do not restate
them. What one setup decides — which directory holds a second checkout — binds nobody, and a file
contributors read as policy does not state it as policy. Where a recipe fixes one of those
choices, the file says so as a fact about the recipe, next to the rule the recipe is serving. What
belongs to your own setup goes somewhere the tree does not track — for Claude Code, a `CLAUDE*.md`
of your own, which `.gitignore` keeps out of the tree.

## Decisions and contracts

What ryo-v2 promises to a caller is its contracts; what it has measured and decided is in the
ledgers (`CHECK_LEDGER.md`, `SEARCH_LEDGER.md`), and a change is checked against both.

**How a decision is made.** A technical question is settled by rating its options against
[CONCEPT.md](CONCEPT.md) and taking the best; the rating and the reason are written down with the
decision. A question goes to the maintainer only when the code, the measurements and the concept
cannot settle it. Until the first release no input or output format carries a compatibility
promise.

**Where it is recorded.**

- A decision that outlives a single change: a row of "Decisions in force" (`SEARCH_LEDGER.md` for
  the search), with the measurement that settled it, the guard that fails when it is broken, when
  to revisit it, and the options rejected.
- A promise a caller may rely on: a clause of a contract, below.
- What a term means: the glossary in `crates/ryo-v2-core/src/domain/mod.rs`. How an adapter
  realises a promise: that module's doc.
- Measurements: the ledgers' tables, with the machine and the date beside each number.

Nothing else is recorded: no list of work to do, of small gaps found on the way, or of ideas for
later. Work that is decided is handed over in a task's contract; what it settles ends up in the
places above.

**Contracts.** A promise a caller may rely on is written as a clause under a `# Contract` heading in
the rustdoc, with the tests that check it named beside it. The convention is stated in
`crates/ryo-v2-core/tests/doc_contract.rs`, and `cargo test` fails when a named test does not
exist. A clause may be tagged `[invariant]` or `[free]`, and clauses use the terms of the glossary
in `crates/ryo-v2-core/src/domain/mod.rs`; the checker states both.

- A clause changes in the same commit as the behaviour it describes, with its named tests.
- Weakening an `[invariant]` clause is a decision: it gets a "Decisions in force" row.
- Retagging a clause from `[free]` to `[invariant]` names its test in the same change.
- The ceilings of the checker only go down.

## Documentation

API detail lives in the rustdoc. `cargo doc` is where a reader goes for what a
type is and what a function promises. When two texts disagree the code wins, and
after that the doc comment: a comment contradicting an issue is a finding about
the issue. [CONCEPT.md](CONCEPT.md) is the exception: where code disagrees with
it, the code is what is wrong.

Four rules for writing that prose.

**State a rule once, where the thing it constrains is defined.** Everywhere
else, link to that statement and write only what this site adds — why it is an
instance of the rule, and what follows here. A second full statement is the copy
nobody edits the day the rule moves.

**Do not write the state of the tree where nothing maintains it.** "Not yet",
"neither has one", "the only caller" are true when written and go false in
silence. Keep the rule and cut the clause about today.

**A number describing a list belongs in the file holding the list, or nowhere.**
The list is the answer and the number is a copy of it. Point at the list
instead.

**A sentence recording that a rule changed stays.** It is a constraint written
in the past tense, and it is what stands between the next reader and undoing the
rule. The test is whether deleting it lets somebody repeat the mistake. If
deleting it costs nothing it is a report on how a change went, and that belongs
in the commit message.

## Development flow

This is repository policy for every implementer, human or agent.

- Each implementer works in its own worktree and branch. Until the code base is functionally
  complete, integration into `main` is a local merge done by the maintainer, conflicts included;
  pull requests are not required in this phase, and the hand-over ends as
  [Pull requests](CONTRIBUTING.md#pull-requests) says.
- In the same phase no issue tracker and no list of work items is kept. The phase is for validating
  the engine's hypotheses fast; what a validation settles is fixed as a contract, and a contract
  that proves wrong is changed. Work to be done is written into a task's contract and handed over
  from there; what the contract stands for is defined in
  [PUBLIC_DEVELOPMENT.md](PUBLIC_DEVELOPMENT.md#role-of-each-public-artifact).
- A decision that code or data must keep (a grouping, a denominator, a threshold) gets a test or a
  ceiling in the same change that makes the decision; prose alone does not keep it. Review asks
  after it (question 7 of the
  [recommended pattern](#working-with-coding-agents--the-recommended-pattern)).
- A verification repository may be used as a sample. Wherever a file names it, the file says that
  it is a verification repository used as a sample.
- Files and commit messages never describe the execution environment: no host, account, provider,
  path or user name. Commands use placeholders such as `<BASE>` and `<OUT>`. What a reader needs
  to read a measurement — core count, memory, and whether the machine was busy — is part of the
  measurement and stays beside it.
- Weak tests are managed as a measured risk, not driven to zero; the procedure, run once per
  milestone, and its results are in `CHECK_LEDGER.md` (section "Test strength (risk)").

## Working with coding agents — the recommended pattern

What this repository tells an agent is `AGENTS.md`, at the root, in the open and
under the name other coding agents already read. What an agent session leaves
behind — personal instructions, local settings — is not tracked; `.gitignore`
takes it.

If you develop here with a coding agent, the loop that works is:

```text
issue -> just worktree-new -> implement -> just check
      -> review locally
      -> commit
      -> git fetch origin -> just pre-push
      -> write the PR body to a file -> hand over push/PR (or the branch to merge)
```

Review the change locally before it goes up, by a person or by an AI agent —
whichever you have. Questions worth asking of every diff:

1. Does it do what its issue asked?
2. May everything in it be published
   ([PUBLIC_DEVELOPMENT.md](PUBLIC_DEVELOPMENT.md#agent-publication-check))?
3. Are the comments beside the code still true?
4. Does each test a changed `# Contract` clause names actually check that
   clause — would it fail if the promise broke?
5. Does the change alter behaviour a `[free]` clause names, or make callers rely
   on it? Then it is no longer free: retag the clause and name a test.
6. Does a glossary `includes:` list change? Then every clause using that term
   now covers the new members: read them.
7. For each decision the change makes, which test or ceiling guards it, and
   would it fail if the decision were broken?

Record who or what reviewed it, or that nobody did, where the change reaches
`main`:

- With a pull request, a GitHub review on it is the record, and the PR body
  says who or what reviewed it when no such review was made.
- With a local merge ([Development flow](#development-flow)), the hand-over
  note says it, and the merge commit carries it in its body:

  ```text
  Reviewed: <who or what reviewed it, or "nobody">
  Verified: <the recipes actually run, and their outcome>
  ```

A merge commit without these lines records a merge nobody can tell was
reviewed.

Releases and publishes (crates, the repository's own settings) are performed by
a human hand.
