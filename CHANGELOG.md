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
  maintainer, open work in the ledgers, a guard for every decision, no execution environment in
  files, test strength managed as a measured risk).
- `CHECK_LEDGER.md`: the sections "Test strength (risk)" and "Open work".
- `corpus/README.md`: the provenance of the fixtures.

### Changed

- Details of private execution environments are removed from the documentation; commands use the
  placeholders `<BASE>` and `<OUT>`.
