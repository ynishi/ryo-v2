# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `ryo-v2 search <WORKSPACE> <REQUEST.json>`: a dry-run search for the best verified changes of one
  decision (beam search over expanded Blueprints, ranked by reward); the `extract-function` and
  `move-item` Blueprint kinds are accepted by `apply` and `search`.
- The H6 harness: `search-recall gen` plants the inverse of a refactor into clean code
  (`corpus/search`), `search-recall run` prints recall@20 of the pruning, the generation rate of
  the expander and the perf trigger status; `SEARCH_LEDGER.md` keeps the numbers.
- The decisions of the search in force (time budget of a decision, revert assertion in production,
  parallelism and memory, triggers for perf work), with their measurements, guards and rejected
  options, in `SEARCH_LEDGER.md` ("Decisions in force"); they replace the former `docs/adr`
  files, which are removed.
- `CONTRIBUTING.md`: the "Development flow" section (worktree per implementer, local merge by the
  maintainer, open work in `CHECK_LEDGER.md`, a guard for every decision, no execution environment
  in files, test strength managed as a measured risk).
- `CHECK_LEDGER.md`: the sections "Test strength (risk)" and "Open work".
- `corpus/README.md`: the provenance of the fixtures.
- `SEARCH_LEDGER.md`: the first search-recall runs on ryo-rs (a verification repository used as a
  large sample): recall, generation, outer check and perf triggers, verdict insufficient;
  `CHECK_LEDGER.md` "Open work" holds what they found.
- `RaSession::expander_panics()`: the panics the expander contained, with edit kind, file and
  message. `search-recall run` prints `contained panics: <n> detector, <n> expander` and one line
  per panic, also in `--markdown` and under `contained_panics` in `--report`.

### Changed

- The oracle prints the contained detector and expander panics and fails when there is any.
- Details of private execution environments are removed from the documentation; commands use the
  placeholders `<BASE>` and `<OUT>`.
- `PUBLIC_DEVELOPMENT.md`, `CONTRIBUTING.md` and `AGENTS.md` agree with the development flow:
  while no issue tracker is used, a `CHECK_LEDGER.md` "Open work" row stands for the issue and the
  hand-over note for the pull request body; the review questions are one numbered list, including
  which test or ceiling guards each decision; branch types include `refactor` and `chore`.
- `docs/bench-m2.md`: the large-run steps export ryo-rs with `git archive` as the run did, with
  placeholders instead of a path.
- `CONTRIBUTING.md`: where a review is recorded — a GitHub review or the PR body, or, with a local
  merge, the hand-over note and `Reviewed:` / `Verified:` lines in the merge commit's body.
- The crate `ryo-v2-cli` is renamed `ryo-v2-host` (`cargo run -p ryo-v2-host`): it is the binary
  that hosts the engine, and the command line is one front of it. The `ryo-v2` binary, its
  subcommands and its output are unchanged.
- Cargo manifests: `missing_docs` is set once in `[workspace.lints]`, inherited by every member;
  the workspace states `rust-version = "1.98"`, inherited by every member, and drops the deprecated `authors`; each publishable
  crate (`ryo-v2`, `ryo-v2-core`, `ryo-v2-ra`, `ryo-v2-host`) has its own `documentation` URL,
  `keywords` and `categories`, and the core's `description` says what the core is.
- Published packages of those crates contain `src/`, the manifest, `README.md` and both licence
  files (`LICENSE-MIT`, `LICENSE-APACHE`), and no longer `tests/`, which need the corpus and the
  workspace root to run. `crates/ryo-v2-core/tests/manifest.rs` keeps these manifest rules.
- `just rust-clippy` denies warnings through Cargo's `build.warnings` (`CARGO_BUILD_WARNINGS=deny`)
  instead of `-- -D warnings`, which invalidated the build cache; `just rust-clippy` and
  `just rust-test` run with `--locked`. `crates/ryo-v2-core/tests/manifest.rs` also checks these
  recipes.

### Fixed

- The expander no longer panics on the value of a struct-literal field that has an attribute
  (`#[cfg(..)] k: v`): extract-variable refuses it, since rust-analyzer would build the variable's
  name from the whole field text. A panic inside the expander now costs one call of one edit kind,
  not every child of the state being expanded.
