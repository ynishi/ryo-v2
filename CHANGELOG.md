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
- The `apply` and `search` JSON list, per candidate, the Blueprints of its path (intent, proposer,
  rationale, spec refs), and under `evidence` the provenance of the decision: analyzer name and
  version, its settings and a workspace fingerprint built from the per-file fingerprints.
- `RaSession::expander_panics()`: the panics the expander contained, with edit kind, file and
  message. `search-recall run` prints `contained panics: <n> detector, <n> expander` and one line
  per panic, also in `--markdown` and under `contained_panics` in `--report`.

### Changed

- `RaConfig.unresolved_paths` is renamed `RaConfig.gap_detectors`: it switches all three gap
  detectors (unresolved-path, duplicate-name, field-bounds), and the Evidence provenance records
  it under `gap_detectors`. The oracle switch `RYO_V2_ORACLE_UNRESOLVED=off` is now
  `RYO_V2_ORACLE_GAP_DETECTORS=off`; the old name stops the run. `DetectorPanic` names its
  detector (`GapDetector`), and the test fault hook `with_detector_fault` takes a detector and a
  file. The contained-panics lines of the harnesses print the detector.
- `search-recall run` reports the time of its outer `cargo check` apart (`outer_check_ms`,
  `search_ms` in the JSON report), and its trigger 4 line follows decision D4 as restated in
  `SEARCH_LEDGER.md`: the run without its outer check is compared with the limit, and a run with a
  budget beyond the default (`--exhaustive`, a wider beam) is not measured.
- The oracle prints the contained detector and expander panics and fails when there is any.
- A Blueprint carries the fingerprint of the whole base text of every file it edits (`BaseFp`), and
  `apply` blocks it as stale when any byte of such a file changed, not only the bytes of a read: an
  identical line inserted above a read no longer passes. Reads of files the Blueprint only reads
  keep their exact-offset check. Blueprint JSON requires `"bases": [{"path", "fingerprint"}]`, so
  input written without it no longer parses; `ryo-v2 fingerprint WORKSPACE PATH` without a range
  prints such an entry; a `stale` blocking lists `bases` beside `reads`; `fingerprints_checked`
  counts the bases and the reads of files without one. `Blueprint::new` takes a `BlueprintParts`.
- A candidate's risk marks and Rule are derived by the domain from its Blueprints and the roles of
  the files they edit; `Candidate`, `Evidence` and decision-log entries can no longer be built
  outside the core crate, and the `Verifier` port gains `provenance`.
- A mutation that writes back the bytes it replaces (rename `a` to `a`) is refused as
  nonconforming before any check.
- A spec reference is `<doc>#<item>`; other text is refused.
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

- The unresolved-path detector no longer reports types defined by a macro rust-analyzer could not
  expand (a proc macro without the server, an `include!` of `OUT_DIR` without build scripts): it
  skips a file with such a macro call. New corpus base `build-lab`.
- The expander no longer panics on the value of a struct-literal field that has an attribute
  (`#[cfg(..)] k: v`): extract-variable refuses it, since rust-analyzer would build the variable's
  name from the whole field text. A panic inside the expander now costs one call of one edit kind,
  not every child of the state being expanded.
- The verifier (`ryo-v2-ra`) rejects two kinds of edit that rust-analyzer passes and `cargo check`
  refuses: a module in which an item and a `use` define the same name (E0255; the move-item
  candidates that put an item into the module still importing it), and a derived impl or a written
  `impl Copy` whose trait a field of the type lacks (E0204, E0277, E0369; add-derive proposes such
  derives). The small-base search-recall run with `--outer-check` goes from 14 of 114 decided
  candidates failing `cargo check` to 0 of 112. The oracle has the groups `duplicate-name`,
  `copy-impl` and `derive-field` with 13 cases on breakage-lab; `RaConfig.unresolved_paths`
  switches the new detectors with the old one.
