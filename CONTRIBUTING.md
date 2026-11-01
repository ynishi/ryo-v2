# Contributing

The basic conventions for changes to this repository, for humans and coding agents alike. How
development is done here (decisions, contracts, documentation, the flow to `main`) is in
[DEVELOPMENT.md](DEVELOPMENT.md); what the product is, in [CONCEPT.md](CONCEPT.md). The disclosure
policy in [PUBLIC_DEVELOPMENT.md](PUBLIC_DEVELOPMENT.md) outranks this file.

## Start with an issue

Open an issue before you change anything, and make the change on its own branch. "Issue" in this
file includes what stands for one while no issue tracker is used — see
[Development flow](DEVELOPMENT.md#development-flow).

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
verify the base before you build on it: it refuses `main` and runs
`base-check` (see [Verification](#verification)). Run it from the main
checkout, and remove the worktree once the branch is merged.

A branch cut from an older `main` can collide with the current one only in the
build after the merge, so merge the current `main` into the branch before the
final check; `just check` refuses the branch until you do.

The ledgers (`CHECK_LEDGER.md`, `SEARCH_LEDGER.md`) gain and lose table rows and
sections on every branch, and a line merge reports a conflict wherever two such
edits touch. `.gitattributes` gives them a merge driver
(`crates/ryo-v2-ledger-merge`) that merges by section and by table row, keyed by
the heading and by the row's first cell: a change made on one side is taken, a
row both sides changed differently gets conflict markers around it alone, and a
section both sides changed is merged line by line inside (a section one side
removed and the other changed is marked as a whole). Which command a driver runs is set in each checkout's own git config,
not in the tree, so a checkout registers it once with
`just ledger-merge-install` (it builds the driver; worktrees of the checkout
share it; run it again after the driver's code changes). A checkout without it,
or a driver that fails, merges the ledgers line by line, as before.
`just ledger-merge-try <base> <ours> <theirs> [<expected>]` runs the driver on
three files without git.

## Verification

`just check` is the definition of green, and CI is where it runs. It checks
the base of the branch, formats, lints with warnings denied, tests, and runs
`cargo deny`.

The base check (`just base-check`) refuses a branch that does not contain
`origin/main`, or the local `main` where there is no `origin/main`. It passes
on `main` itself. Where it cannot tell — outside a git checkout of the tree (a
copy without its history), in a shallow clone, or with neither ref — it says in
one line that it skipped, and passes. The tests include that no file
`.gitattributes` routes to the ledger merge driver holds a conflict marker.

Report what was actually run; "I did not verify X" is a usable report, a green
claim resting on a recipe nobody ran is not.

## Preferred commit format

```text
<subject: what changed, one line>

<prose: the problem, why this fix and not the alternative, what it
cost. This is where the reasoning lives.>

Verified: <the recipes actually run, and their outcome>

Refs <issue: #<n>, a decision such as D4, or the contract clause it changes>
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

## Pull requests

While the maintainer merges locally ([Development flow](DEVELOPMENT.md#development-flow)),
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

## License and security

The license is stated in [README.md](README.md#license). A suspected vulnerability is reported as
[SECURITY.md](SECURITY.md) says, not in a public issue.
