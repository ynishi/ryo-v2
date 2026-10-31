# Contributing

Shared conventions for changes to this repository, for humans and coding agents
alike. The disclosure policy in [PUBLIC_DEVELOPMENT.md](PUBLIC_DEVELOPMENT.md)
outranks this file.

Three kinds of statement share this file, and the boundary between them is a
rule of its own. What the repository decides binds everyone who contributes:
never on `main`, the commit format, what a gate is and when it runs. What
installed tooling decides travels with the tool: a plugin's agents behave the
same in every repository that installs them, and this file does not restate
them. What one setup decides — which directory holds a second
checkout — binds nobody, and a file contributors read as policy does not state
it as policy. Where a recipe here fixes one of those choices, this file says so
as a fact about the recipe, next to the rule the recipe is serving. What belongs
to your own setup goes somewhere the tree does not track — for Claude Code, a
`CLAUDE*.md` of your own, which `.gitignore` keeps out of the tree.

## Issue conventions

"Issue" in this file includes what stands for one while no issue tracker is
used — see [Development flow](#development-flow).

### Labels

Four categories. Assign at least one when you open an issue.

| Label         | The change                                           |
| ------------- | ---------------------------------------------------- |
| `bug`         | behaviour contradicts what it promises               |
| `enhancement` | behaviour that does not exist yet                    |
| `refactor`    | behaviour unchanged                                  |
| `chore`       | production code untouched — CI, tests, tooling, docs |

A ledger row carries no label; the `<type>` of its branch (see
[Branches](#branches)) is its category.

## Branches

Never work on `main`. One branch per issue, cut from `origin/main` after a fetch
and named `<type>/<slug>`, where `<type>` is `feat`, `fix`, `refactor`, `chore`,
`ci` or `docs`.

Whether that branch lives in a second worktree or in the checkout you already
have is your setup's decision. If the second checkout sits inside this tree, it
sits where `.gitignore` already covers, so the checkout containing it stays
clean.

Here a recipe makes that choice, and this is what it chooses:

```bash
just worktree-new <type> <slug>
```

That is `git fetch origin`, then a `git worktree add` of a new `<type>/<slug>`
branch cut from `origin/main` — one worktree per issue, in a directory
`.gitignore` covers (the `Justfile` names it) — then `just branch-check` to
verify the base before you build on it. Run it from the main checkout, and
remove the worktree once the branch is merged.

## Verification

`just check` is the definition of green, and CI is where it runs. It formats,
lints with warnings denied, tests, and runs `cargo deny`.

Report what was actually run; "I did not verify X" is a usable report, a green
claim resting on a recipe nobody ran is not.

## Documentation

API detail lives in the rustdoc. `cargo doc` is where a reader goes for what a
type is and what a function promises. When two texts disagree the code wins, and
after that the doc comment: a comment contradicting an issue is a finding about
the issue.

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

A promise a caller may rely on is written as a clause under a `# Contract`
heading in the rustdoc, with the tests that check it named beside it. The
convention is stated in `crates/ryo-v2-core/tests/doc_contract.rs`, and
`cargo test` fails when a named test does not exist. A clause may be tagged
`[invariant]` or `[free]`, and clauses use the terms of the glossary in
`crates/ryo-v2-core/src/domain/mod.rs`; the checker states both.

Decisions that outlive a single change are recorded in the "Decisions in force" section of
`SEARCH_LEDGER.md`, one row per decision, with the measurement that settled it.

## Preferred commit format

```text
<subject: what changed, one line>

<prose: the problem, why this fix and not the alternative, what it
cost. This is where the reasoning lives.>

Verified: <the recipes actually run, and their outcome>

Refs <issue: #<n>, or a ledger id such as S-2>
```

- No AI attribution of any kind — no `Co-Authored-By`, no "Generated with", no
  `Signed-off-by`. `just commit-msg-check` looks for them.
- `cargo fmt` output and clippy fixes go in their own commits, separate from
  behaviour changes.
- Do not edit `CHANGELOG.md` by hand. It is generated from the commit messages when a release is
  cut, so the subject and body of each commit are what the changelog will say. This replaced the
  rule of a hand-written `## [Unreleased]` entry per change, which conflicted in nearly every
  merge of parallel branches and duplicated the commit messages.
- Never commit working notes, another checkout, or local agent state; `.gitignore` names them. If a commit needs `git add -f`, stop:
  something is filed wrong.
- A pull request title reaches `main`'s merge commit, and GitHub reads a CI skip
  keyword there the same way it reads one in a message: the run is skipped, its
  checks stay _pending_ rather than failing, and nothing looks wrong. Write "the
  skip keyword" instead, or name the mechanism.

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

## Pull requests

While the maintainer merges locally ([Development flow](#development-flow)),
step 3 is replaced by naming the branch to merge; steps 1 and 2 stay, with the
body as the note the merge is made from.

An agent's part ends with everything that does not write to anything remote, and
that includes the gate:

1. `git fetch origin`, then `just pre-push` — the agent runs both. `pre-push` is
   `branch-check`, `commit-msg-check` and `check`. None of them writes to
   anything remote, so being denied `git push` is no reason to skip it. Report
   the result.
2. **Write the PR body to a file** outside the tree, and say where it is — so
   that the command handed over can be `--body-file <path>` and the human reads
   the same bytes that will be posted. Where the file lives is your setup's
   choice; that it is a file, and not a summary in the chat, is the rule.
3. Hand over the literal commands. These two are the only ones that write to
   anything remote:

   ```bash
   git push -u origin <branch>
   gh pr create --base main --head <branch> \
     --title "<subject>" --body-file <path>
   ```

The PR body records what changed, what was verified — or a statement that
it was not — and what it
deliberately does not cover, under the same disclosure policy as everything
else.

## Development flow

This is repository policy for every implementer, human or agent.

- Each implementer works in its own worktree and branch. Until the code base is functionally
  complete, integration into `main` is a local merge done by the maintainer, conflicts included;
  pull requests are not required in this phase, and the hand-over ends as
  [Pull requests](#pull-requests) says.
- In the same phase no issue tracker is used. Work to be done is written into the "Open work"
  section of `CHECK_LEDGER.md` or into the task's contract, and handed over from there; what that
  row stands for is defined in
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
