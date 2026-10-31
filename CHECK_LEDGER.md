# Check ledger

The gaps between ryo-v2's in-memory verification (`ryo-v2-ra`, rust-analyzer HIR
diagnostics) and the oracle (`cargo check` run once, kept under
`corpus/<ws>/expected/`), as the oracle harness (`crates/ryo-v2-oracle`,
`tests/oracle.rs`) finds them. One row is one kind of gap with its count and
examples; the root cause names the domain concept that is inconsistent
(CodeState, Edit, Scope, Baseline, CrateGraph, Diagnostic, Verdict), not the
symptom. A floor is raised, never lowered without a row here.

Run: `just oracle` (`RYO_V2_ORACLE_REPORT=<file>` writes every outcome).

## Test strength (risk)

Weak tests are managed as a measured risk, not driven to zero. Once per milestone two things are
done. (a) For each main capability, a change that disables it is made, and some test must fail;
a capability for which no test fails is recorded here as having no failing test and gets a test
before the milestone closes. (b) A sample of n contract clauses (the `# Contract` clauses of the
rustdoc) each gets a mutant that breaks the promise, and the share k/n of the clause's named tests
that still pass with the mutant in place is recorded with its 95 % upper confidence bound (with 0
of n passing, the bound is about 3/n, the rule of three). A high share says the named tests do not
check the promise they are named for.

| milestone | date | capabilities checked | capabilities with no failing test | clauses sampled n | named tests still passing k | k/n (95% upper bound) |
|---|---|---|---|---|---|---|
| M4 | - | - | - | - | - | not measured |

## Open work

Work that is known and not done, handed over from here. One row is one item; a row is removed, with
the change that does the work, when its direction is carried out.

| id | area | what is wrong | direction | state |
|---|---|---|---|---|
| M-1 | rules / intents (`intent.rs`, `resolution.rs`) | The conformance check of the rename-local and add-derive intents is a string match only, and the intents carry no risk mark. A Rule can therefore make a rename inside a string literal, a name capture, one side of a shadowing pair or a struct-field rename auto-applicable. | The engine generates the specs from the intent and its position; until then neither intent may be covered by a Rule. | open: valve in place (`Rule::register` refuses both with `RuleError::TextMatchOnly`, so both end as NeedsReview; guarded by `domain::rule::tests::text_match_intents_cannot_be_registered`); engine-generated specs still open |
| M-2 | candidates | A decided candidate carries whole-file texts (`Candidate::edits`, `edits[].new_text` of the output) and no specs, read ranges or fingerprint of the text it was checked against. ryo-v2 writes nothing itself, and the session is a snapshot of the load, so whoever writes a candidate later cannot tell whether a file changed after the load, and writing it overwrites such a change, inside its ranges or outside them. | Candidates carry specs and read ranges; freshness is re-checked against the files at apply time and the change is spliced in. | open |
| M-3 | Blueprints | Blueprints have no dependencies or conflicts. Each Blueprint of a set is checked alone against the loaded text, so two passing Blueprints on the same file are returned as two whole-file texts, and writing both keeps only the later one. Two passing Blueprints are never checked together, so on different files as well, each passing alone says nothing about both applied. A Blueprint cannot state that its specs are against another Blueprint's result; only the search composes a child onto its parent. | `deps` / `conflicts` on Blueprint, checked in BlueprintSet (acyclic, no overlapping ranges in one file). | open |
| M-4 | freshness | A read-range fingerprint covers only the bytes of the range (`ReadRange::check` C1). An offset names a position only while the bytes before it are unchanged, so a change above that keeps the range's bytes (an identical line inserted above, for one) leaves the read Fresh while the old offset points at another statement. The base text that the specs' offsets refer to has no fingerprint of its own. | A whole-file fingerprint for edited files; anchors around read-only ranges. | open |
| M-6 | verifier gaps | The verifier's known gaps are reflected only by the Borrow mark. `OUTER_CHECK_MAX` is defined but unused, so the outer `cargo check` of the final candidates is not wired. | Derive the risk marks from the weak groups of this ledger; select the outer-check candidates by `OUTER_CHECK_MAX`. | open |
| M-7 | risk marks | Files included from `build.rs` by `mod`, the crates a proc-macro crate depends on and `include!` targets carry no mark. A Blueprint that only reads `Cargo.toml` fails as MissingFile. | Mark these files and crates; let a Blueprint read `Cargo.toml` without a MissingFile failure. | open |
| M-8 | errors | One Blueprint's CheckError stops the whole set. | Split per-Blueprint errors from session-fatal ones. | open |
| M-11 | domain visibility (stages) | The marks and the Rule of a candidate are derived in the domain and `Candidate::new` is `pub(crate)`, but the stages before the check (file role, freshness, conformance, splice) still run in `app::apply::prepare`: `Candidate::new` trusts its callers inside the crate to have run them. `NearMiss` keeps `pub` fields. | Make the stages a domain function whose result is the only input `Candidate::new` accepts (with the candidate specs of M-2); make the `NearMiss` fields private. | open |
| M-12 | reproducibility (provenance) | `Evidence` carries a `Provenance`: analyzer name and version, four settings of the adapter (`proc_macro`, `cfg_test`, `unresolved_paths`, `check_timeout_ms`) and a workspace fingerprint built from the fingerprints of the indexed `.rs` files in path order. Manifests, the lock file, the toolchain and the sysroot are not in it, so two runs with the same fingerprint can still see different dependencies. The adapter's `provenance()` has no test of its own (its clause names only the test that pins the version constant to the manifest): its analyzer name, its config keys, the files it fingerprints and its fallback `Provenance::unrecorded` (analyzer `unknown`, taken when `Provenance::new` refuses) are unguarded. | Add the manifest and lock-file fingerprints and the toolchain version as values from the adapter (with the per-file base fingerprints of M-4); a test in the adapter's verifier tests that a loaded session's provenance names `rust-analyzer` at the pinned version, lists the four settings and fingerprints every indexed file. | open |
| R-2 | unresolved-path detector | The detector reports types generated by macros as unresolved when proc-macros or `OUT_DIR` are not available. | Skip files with unresolved macro calls. | open |
| R-5 | harness reporting | A detector panic is now contained per file and recorded in `RaSession::detector_panics()`, but neither the oracle nor the search-recall harness prints it, so a run with contained panics looks the same as one without. | Print the recorded detector panics in both harnesses and fail the oracle when any are recorded; print as well the panics a search-recall run goes on past outside the detector, such as the expander's (S-3). | open |
| R-4 | diagnostic matching | A finding on the same code and line as a rust-analyzer error is dropped even when it is a different path. The imported-name exemption is per file. The baseline key includes the full message, so editing a type name changes an existing error's message and makes it "new". `ForkInUse`, `AlreadyApplied` and `Load` are mapped to Poisoned. | Match findings on code, line and path; scope the exemption per use; key the baseline without the type names in the message; map the three errors to their own CheckError variants. | open |
| A-1 | arch-lint | The id-generation deny of arch-lint matches only written paths (`uuid::Uuid::new_v7(..)`), not the imported form (`use uuid::Uuid; Uuid::new_v7(..)`). | Match the imported form as well. | open |
| S-2 | search metric | The extensibility term of the score is 0 for every candidate: the syntax assessor counts change-probe sites (matches without a catch-all arm, record literals and patterns without `..`, trait impls), and none of the expander's edit kinds (rename, extract, inline, move between files) adds or removes such a site. Probes are written for every planted case, but they cannot change the ranking (SEARCH_LEDGER "Probes"). | An expander edit kind that changes a site count, or a different extensibility metric (with M5's spec model); until then the term is reported as inert. | open |
| S-3 | expander (edit kinds that build a name) | Each of the three search-recall runs on ryo-rs (a verification repository used as a large sample, SEARCH_LEDGER "ryo-rs (large run)", 2026-11-01) hit one panic inside rust-analyzer's syntax factory: `ra_ap_syntax-0.0.358 src/ast/make.rs:135: Failed to make ast node Name from text 'mod #[cfg(feature = "workspace")] dep_resolver: std::sync::OnceLock::new();'`. The text of a cfg-gated field of a struct literal reached a constructor of a `Name`. Every run went on past the panic, and the harness does not count it (R-5), so the run's tables do not show it. The likely producer is an edit kind that builds a name from item text (misplace-item / move-item on a cfg-gated field); not verified. | Find the producer with a case built from that struct literal; make it refuse text that is not an identifier instead of panicking; count such panics in the harness (R-5). | open |
| S-4 | expander (generation) | On ryo-rs (a verification repository used as a large sample, SEARCH_LEDGER "ryo-rs (large run)") the best of five of the 11 planted decisions is never generated: `inline-variable-0` (no candidate at all), `inline-variable-2`, `misplace-item-0`, `misplace-item-1` and the pair `obscure-name` plus `inline-variable`. Generation is 0.55 (0.79 on the small bases). In the exhaustive run the pair keeps every depth-1 child and still does not generate its best, so the miss is the expander's. The cause is not investigated; whether the inline-variable ones are the `let`-initializer gap (SEARCH_LEDGER "Generation") is not checked. | Find, for each of the five, the edit the expander would have to propose and why it does not; a case on the small bases for each cause found. | open |
| M-10 | verifier gaps | The outer `cargo check` of the ryo-rs run (a verification repository used as a large sample, SEARCH_LEDGER "Outer check") rejects 2 of 30 decided candidates that rust-analyzer's diagnostics passed: E0255 for candidate 0 of `misplace-item-0` (at `crates/ryo-agent/src/decide.rs:147` of ryo-rs) and E0204 for candidate 1 of `misplace-item-2` (at `crates/ryo-agent/src/decide.rs:150`). E0255 is the code of the 14 failures of the small bases; E0204 is not among them. The outer check of the final candidates is not wired outside the harness (M-6), so such a candidate can be decided. | Add both to the oracle corpus as cases and give each a Gaps row; wire the outer check (M-6). | open |
| S-5 | perf triggers (decision D4) | Trigger 4 (one recall run over 600,000 ms) was printed as fired only by the ryo-rs run with `--outer-check` (983,033 ms; SEARCH_LEDGER "Perf triggers"). The decisions of that run take about as long as those of the run without it (360,538 ms, not fired); the extra time is the outer `cargo check` of each decided candidate, outside the search. D4 states trigger 4 for one recall run in CI, which none of these runs was, so by its text no perf task is opened; it does not say whether outer-check time counts, and the harness prints the trigger as fired for any run over the limit. | Restate trigger 4 in a new D4 row with this measurement (whether outer-check time counts, and which run is the one judged); then make the harness's trigger 4 line follow it, with a test that pins the fired line (only the not-fired line is tested today). | open |

## Counts

Corpus: 4 workspaces, `base/extract-trait-multicrate` (8 hand cases + 18
breakage cases), `base/breakage-lab` (38 breakage cases), `base/macro-lab`
(11 hand cases: a local proc-macro crate, a derive, two attribute macros and a
`macro_rules!`) and `base/cfg-test-lab` (5 hand cases: errors inside a
`#[cfg(test)]` module and an integration test), 80 cases in all (`just oracle`;
expected from `just oracle-refresh`; breakage cases from `just breakage-gen`,
groups from `just breakage-regroup`). Groups with no case (arity) are omitted.
oracle-refresh over all bases on 2026-10-31 (`just oracle-refresh-all`, every case, after the
refusal of errors without a code): no codeless error, expected files unchanged.

Expected diagnostics are `cargo check --workspace --all-targets --keep-going`
(since the V2 change below; before, `--workspace` alone, lib and bins). The
tables from "after: V2" down are the current ones; the older tables ("after:
HEAD 1d5a2a6" and below) are the history of the 75-case corpus and stay as they
were.

Definition (since the macro-lab change). Per compared (not skipped) case, over
the expected diagnostics whose code is in the case's group (`Group::codes()`):
`of` counts the cases that have at least one; `hit` counts those where none of
them was missed (extra and unmapped diagnostics do not matter); recall = hit /
of. `exact` is the old pass (the reported set equals the expected set); `FP` is
the number of cases with an extra or unmapped diagnostic of any code;
precision = distinct group-code diagnostics reported and expected / (those +
distinct group-code diagnostics reported and not expected). Every missed or
extra diagnostic whose code is outside the group of its case, and every
unmapped one (as `ra:<code>`), goes to the "other codes" table. The floors
were re-based from `pass` to `hit` (`exact` for valid); no group's floor went
down (unresolved-import 3 to 4, no-method 9 to 10, type-mismatch 3 to 7, valid
4 to 5 with the new macro-valid-0; the five macro groups are new). The
unresolved-path detector (`ryo-v2-ra/src/unresolved.rs`) then raised
unresolved-import 4 to 7 and unresolved-name 0 to 5, the measured `hit`; the
floors are checked only with proc-macro on and the detector on
(`RYO_V2_ORACLE_UNRESOLVED=off` skips them).

### after: V2 (oracle truth = the CI gate), cfg-test on, proc-macro on

Command: `just oracle` (`RaConfig.cfg_test = true`, expected from `--all-targets`).
Floors hold; no case is MISGROUPED or skipped. The floor column is the checked
floor. Since `valid-02` moved from the valid group to cfg-test (its overlay breaks
the lib test module, E0599 lib/src/lib.rs:182), the cfg-test and valid rows are
remeasured and their floors equal the measured counts (cfg-test `hit` 4 to 5,
valid `exact` 5). An unfiltered run also asserts the H7 denominator: primary
`of` at least `PRIMARY_OF_FLOOR` (32), no skipped case and no misgrouped case
(`Report::ratchet_problems`; a valid case with an expected error is misgrouped).

| group | cases | skip | of | hit | FN | exact | FP | floor | recall | precision |
|---|---|---|---|---|---|---|---|---|---|---|
| unresolved-import | 7 | 0 | 7 | 7 | 0 | 5 | 2 | 7 | 1.00 | 0.91 |
| unresolved-name | 5 | 0 | 5 | 5 | 0 | 2 | 1 | 5 | 1.00 | 1.00 |
| unresolved-type | 2 | 0 | 2 | 0 | 2 | 0 | 1 | 0 | 0.00 | - |
| no-method | 10 | 0 | 10 | 10 | 0 | 9 | 1 | 10 | 1.00 | 0.93 |
| type-mismatch | 7 | 0 | 7 | 7 | 0 | 3 | 4 | 7 | 1.00 | 0.71 |
| trait-bound | 3 | 0 | 3 | 3 | 0 | 3 | 0 | 3 | 1.00 | 1.00 |
| field | 7 | 0 | 7 | 1 | 6 | 0 | 7 | 1 | 0.14 | 0.14 |
| borrow | 10 | 0 | 10 | 0 | 10 | 0 | 0 | 0 | 0.00 | - |
| privacy | 5 | 0 | 5 | 4 | 1 | 4 | 0 | 4 | 0.80 | 1.00 |
| const-eval | 2 | 0 | 2 | 0 | 2 | 0 | 0 | 0 | 0.00 | - |
| cfg | 2 | 0 | 2 | 1 | 1 | 1 | 0 | 1 | 0.50 | 1.00 |
| cfg-test | 5 | 0 | 5 | 5 | 0 | 4 | 1 | 5 | 1.00 | 0.83 |
| macro-derive | 2 | 0 | 2 | 2 | 0 | 2 | 0 | 2 | 1.00 | 1.00 |
| macro-attr | 2 | 0 | 2 | 2 | 0 | 2 | 0 | 2 | 1.00 | 1.00 |
| macro-rules | 2 | 0 | 2 | 0 | 2 | 0 | 2 | 0 | 0.00 | 0.20 |
| macro-tokens | 3 | 0 | 3 | 2 | 1 | 2 | 1 | 2 | 0.67 | 0.75 |
| proc-macro-edit | 1 | 0 | 1 | 0 | 1 | 0 | 0 | 0 | 0.00 | - |
| valid | 5 | 0 | 0 | 0 | 0 | 5 | 0 | 5 | - | - |

| other code | missed | extra | cases |
|---|---|---|---|
| E0277 | 0 | 4 | 3 |
| E0282 | 0 | 2 | 2 |
| E0392 | 1 | 0 | 1 |
| E0422 | 4 | 0 | 2 |
| E0599 | 0 | 2 | 2 |

H7 primary recall 32/32 = 1.00. The same run with the proc-macro server off
(`just oracle-off`, cfg-test on) differs from the table above only in
macro-derive (hit 0, exact 0, FP 0), macro-attr (hit 1, exact 1, FP 1, precision
0.50), as in the proc-macro-off table below.

Against the table of the 75-case corpus ("after: HEAD 1d5a2a6 ... proc-macro
on"): every old row keeps its `hit` and its floor. What changed is `exact` and
FP, from diagnostics that the all-targets expected files added and ra reports
differently: field FP 6 to 7 (extract-trait-multicrate broken-06, G-A-2),
field exact 1 to 0, unresolved-name exact 4 to 2 (remove-import-1, -2 now expect
an E0422 that ra does not report, G-B-5), and the precision cells that follow.
The new rows are cfg-test (5 cases of the new workspace, 4 in the group, 1
valid) and valid 5 to 6 (`cfgtest-valid-0`); with `valid-02` moved, cfg-test has
5 cases (the 4 of cfg-test-lab and `valid-02`, all hit) and valid 5 again.

### after: V2, cfg-test off, proc-macro on

Command: `RYO_V2_ORACLE_CFG_TEST=off just oracle`. Floors are not checked
(the floor column is the on value). This is ra as it was before the change
(`CargoConfig::set_test` false) against the new expected files. This table and
the older ones were measured with `valid-02` in the valid group.

| group | cases | skip | of | hit | FN | exact | FP | floor | recall | precision |
|---|---|---|---|---|---|---|---|---|---|---|
| unresolved-import | 7 | 0 | 7 | 6 | 1 | 4 | 2 | 7 | 0.86 | 0.90 |
| unresolved-name | 5 | 0 | 5 | 4 | 1 | 2 | 1 | 5 | 0.80 | 1.00 |
| unresolved-type | 2 | 0 | 2 | 0 | 2 | 0 | 1 | 0 | 0.00 | - |
| no-method | 10 | 0 | 10 | 6 | 4 | 5 | 1 | 10 | 0.60 | 0.91 |
| type-mismatch | 7 | 0 | 7 | 6 | 1 | 3 | 4 | 7 | 0.86 | 0.69 |
| trait-bound | 3 | 0 | 3 | 3 | 0 | 3 | 0 | 3 | 1.00 | 1.00 |
| field | 7 | 0 | 7 | 1 | 6 | 1 | 6 | 1 | 0.14 | 0.07 |
| borrow | 10 | 0 | 10 | 0 | 10 | 0 | 0 | 0 | 0.00 | - |
| privacy | 5 | 0 | 5 | 4 | 1 | 4 | 0 | 4 | 0.80 | 1.00 |
| const-eval | 2 | 0 | 2 | 0 | 2 | 0 | 0 | 0 | 0.00 | - |
| cfg | 2 | 0 | 2 | 1 | 1 | 1 | 0 | 1 | 0.50 | 1.00 |
| cfg-test | 4 | 0 | 4 | 0 | 4 | 0 | 0 | 4 | 0.00 | - |
| macro-derive | 2 | 0 | 2 | 2 | 0 | 2 | 0 | 2 | 1.00 | 1.00 |
| macro-attr | 2 | 0 | 2 | 2 | 0 | 2 | 0 | 2 | 1.00 | 1.00 |
| macro-rules | 2 | 0 | 2 | 0 | 2 | 0 | 2 | 0 | 0.00 | 0.20 |
| macro-tokens | 3 | 0 | 3 | 2 | 1 | 2 | 1 | 2 | 0.67 | 0.75 |
| proc-macro-edit | 1 | 0 | 1 | 0 | 1 | 0 | 0 | 0 | 0.00 | - |
| valid | 6 | 0 | 0 | 0 | 0 | 5 | 0 | 5 | - | - |

| other code | missed | extra | cases |
|---|---|---|---|
| E0277 | 2 | 4 | 5 |
| E0282 | 0 | 2 | 2 |
| E0392 | 1 | 0 | 1 |
| E0422 | 4 | 0 | 2 |
| E0599 | 2 | 2 | 3 |

H7 primary recall 25/32 = 0.78. With cfg-test off ra misses the errors inside
test code that the all-targets expected files now hold: the four cfg-test cases
(`cfgtest-mod-0`, `-mod-1`, `-it-0`, `-it-1`: hit 0 of 4, against 4 of 4 with it
on), and the 18 diagnostics added to extract-trait-multicrate (all inside
`#[cfg(test)]` modules), which account for no-method 6 of 10, unresolved-import
6 of 7, unresolved-name 4 of 5, type-mismatch 6 of 7, and for `valid-02` (its
overlay breaks the lib test module, E0599 at lib/src/lib.rs:182: exact only with
cfg-test on). The two integration-test cases are missed with it off as well; the
cause was not isolated (their errors sit inside `#[test]` functions). This is the
measured proof of G-C-1 below.

### changed expected (V2: `--all-targets --keep-going`, `just oracle-refresh`)

Counted against the expected files of HEAD 1a6d103, copied before the refresh.

| workspace | expected files | changed files | diags before | diags after | added | removed |
|---|---|---|---|---|---|---|
| extract-trait-multicrate | 26 | 14 | 51 | 69 | 18 | 0 |
| breakage-lab | 38 | 0 | 52 | 52 | 0 | 0 |
| macro-lab | 11 | 0 | 14 | 14 | 0 | 0 |
| cfg-test-lab (new) | 5 | - | - | 5 | - | - |

The 18 added diagnostics are at 14 places, all inside the `#[cfg(test)]` modules
of the workspace's own crates (lib/src/lib.rs:179-218, user/src/lib.rs:57-80,
viewer/src/lib.rs:24): the lib, user and viewer test modules call the
functions the breakage cases change. Nothing was removed: breakage-lab and
macro-lab have no test code. Without `--keep-going` a refresh and two later
manual runs of cfg-test-lab `cfgtest-it-1` reported different sets (cargo stops
scheduling units after the first failed one), which is why the flag is part of
`CHECK_ARGS`; for the three older workspaces the table above did not change
with it.

Without a kept list, `just breakage-regroup` on the two breakage workspaces moves 5 cases
(extract-trait-multicrate change-type-0 and -1: type-mismatch to trait-bound,
remove-import-1 and -2: unresolved-name to unresolved-type, typo-type-1:
unresolved-name to no-method), because the test-module errors change which
group has the highest-priority code. It is **not applied**: with the moved
groups the checked floors fail (unresolved-name hit 2 < 5, type-mismatch hit
5 < 7; no case lost a hit, the cases left the group). The groups of those 5
cases are therefore the old ones, and the table above is measured with them.
A floor is raised, never lowered. **Decided (2026-10-31): keep the old
groups.** A case's group names what its edit breaks; the errors that
`--all-targets` adds are follow-on errors in the crate's `#[cfg(test)]` code
and must not reclassify the case. They are counted in the other-codes table.
`breakage gen` keeps an existing group, so a later gen does not undo this. The
kept groups are recorded in `KEPT_GROUPS` of the `breakage` bin: `breakage
regroup` prints such a case as kept and leaves its group, and
`corpus_regroup_moves_nothing` checks the corpus (no breakage case would move,
and every entry still names a case that regroup would otherwise move).

### with the old expected files (before V2), cfg-test off, proc-macro on

Command: the corpus of HEAD 1a6d103 (`git archive`, `RYO_V2_CORPUS`) with
`RYO_V2_ORACLE_CFG_TEST=off just oracle`. It equals the table "after: HEAD
1d5a2a6 plus the unresolved-path detector, proc-macro on" row for row, other
codes included (H7 32/32), so the old numbers are reproducible and V2 is a change
of the expected files and of ra's setting, not of the code.

### after: HEAD 1d5a2a6 plus the unresolved-path detector, proc-macro on

| group | cases | skip | of | hit | FN | exact | FP | floor | recall | precision |
|---|---|---|---|---|---|---|---|---|---|---|
| unresolved-import | 7 | 0 | 7 | 7 | 0 | 5 | 2 | 7 | 1.00 | 0.90 |
| unresolved-name | 5 | 0 | 5 | 5 | 0 | 4 | 1 | 5 | 1.00 | 1.00 |
| unresolved-type | 2 | 0 | 2 | 0 | 2 | 0 | 1 | 0 | 0.00 | - |
| no-method | 10 | 0 | 10 | 10 | 0 | 9 | 1 | 10 | 1.00 | 0.91 |
| type-mismatch | 7 | 0 | 7 | 7 | 0 | 3 | 4 | 7 | 1.00 | 0.69 |
| trait-bound | 3 | 0 | 3 | 3 | 0 | 3 | 0 | 3 | 1.00 | 1.00 |
| field | 7 | 0 | 7 | 1 | 6 | 1 | 6 | 1 | 0.14 | 0.07 |
| borrow | 10 | 0 | 10 | 0 | 10 | 0 | 0 | 0 | 0.00 | - |
| privacy | 5 | 0 | 5 | 4 | 1 | 4 | 0 | 4 | 0.80 | 1.00 |
| const-eval | 2 | 0 | 2 | 0 | 2 | 0 | 0 | 0 | 0.00 | - |
| cfg | 2 | 0 | 2 | 1 | 1 | 1 | 0 | 1 | 0.50 | 1.00 |
| macro-derive | 2 | 0 | 2 | 2 | 0 | 2 | 0 | 2 | 1.00 | 1.00 |
| macro-attr | 2 | 0 | 2 | 2 | 0 | 2 | 0 | 2 | 1.00 | 1.00 |
| macro-rules | 2 | 0 | 2 | 0 | 2 | 0 | 2 | 0 | 0.00 | 0.20 |
| macro-tokens | 3 | 0 | 3 | 2 | 1 | 2 | 1 | 2 | 0.67 | 0.75 |
| proc-macro-edit | 1 | 0 | 1 | 0 | 1 | 0 | 0 | 0 | 0.00 | - |
| valid | 5 | 0 | 0 | 0 | 0 | 5 | 0 | 5 | - | - |

The FP column equals the before table in every row: the detector adds no extra
diagnostic (E0405, E0425 or E0433) to any case.

### after: HEAD 1d5a2a6 plus the unresolved-path detector, proc-macro off

Floors are not checked with proc-macro off (the floor column is the on value).

| group | cases | skip | of | hit | FN | exact | FP | floor | recall | precision |
|---|---|---|---|---|---|---|---|---|---|---|
| unresolved-import | 7 | 0 | 7 | 7 | 0 | 5 | 2 | 7 | 1.00 | 0.90 |
| unresolved-name | 5 | 0 | 5 | 5 | 0 | 4 | 1 | 5 | 1.00 | 1.00 |
| unresolved-type | 2 | 0 | 2 | 0 | 2 | 0 | 1 | 0 | 0.00 | - |
| no-method | 10 | 0 | 10 | 10 | 0 | 9 | 1 | 10 | 1.00 | 0.91 |
| type-mismatch | 7 | 0 | 7 | 7 | 0 | 3 | 4 | 7 | 1.00 | 0.69 |
| trait-bound | 3 | 0 | 3 | 3 | 0 | 3 | 0 | 3 | 1.00 | 1.00 |
| field | 7 | 0 | 7 | 1 | 6 | 1 | 6 | 1 | 0.14 | 0.07 |
| borrow | 10 | 0 | 10 | 0 | 10 | 0 | 0 | 0 | 0.00 | - |
| privacy | 5 | 0 | 5 | 4 | 1 | 4 | 0 | 4 | 0.80 | 1.00 |
| const-eval | 2 | 0 | 2 | 0 | 2 | 0 | 0 | 0 | 0.00 | - |
| cfg | 2 | 0 | 2 | 1 | 1 | 1 | 0 | 1 | 0.50 | 1.00 |
| macro-derive | 2 | 0 | 2 | 0 | 2 | 0 | 0 | 2 | 0.00 | - |
| macro-attr | 2 | 0 | 2 | 1 | 1 | 1 | 1 | 2 | 0.50 | 0.50 |
| macro-rules | 2 | 0 | 2 | 0 | 2 | 0 | 2 | 0 | 0.00 | 0.20 |
| macro-tokens | 3 | 0 | 3 | 2 | 1 | 2 | 1 | 2 | 0.67 | 0.75 |
| proc-macro-edit | 1 | 0 | 1 | 0 | 1 | 0 | 0 | 0 | 0.00 | - |
| valid | 5 | 0 | 0 | 0 | 0 | 5 | 0 | 5 | - | - |

### after: other codes (detector on, proc-macro on; the same off)

| other code | missed | extra | cases |
|---|---|---|---|
| E0277 | 0 | 4 | 3 |
| E0282 | 0 | 2 | 2 |
| E0392 | 1 | 0 | 1 |
| E0599 | 0 | 2 | 2 |

E0405, E0425 and E0433 are gone from the table (missed 1, 1 and 4 before).

### with `RYO_V2_ORACLE_UNRESOLVED=off`, proc-macro on

The detector off gives the before table below, row for row (the floor column
shows the raised floors, which are not checked in this mode), and the other
codes table with E0405 1, E0425 1 and E0433 4 missed; H7 is 24/32 = 0.75.

| group | cases | skip | of | hit | FN | exact | FP | floor | recall | precision |
|---|---|---|---|---|---|---|---|---|---|---|
| unresolved-import | 7 | 0 | 7 | 4 | 3 | 3 | 2 | 7 | 0.57 | 0.80 |
| unresolved-name | 5 | 0 | 5 | 0 | 5 | 0 | 1 | 5 | 0.00 | - |
| unresolved-type | 2 | 0 | 2 | 0 | 2 | 0 | 1 | 0 | 0.00 | - |
| no-method | 10 | 0 | 10 | 10 | 0 | 9 | 1 | 10 | 1.00 | 0.91 |
| type-mismatch | 7 | 0 | 7 | 7 | 0 | 3 | 4 | 7 | 1.00 | 0.69 |
| trait-bound | 3 | 0 | 3 | 3 | 0 | 3 | 0 | 3 | 1.00 | 1.00 |
| field | 7 | 0 | 7 | 1 | 6 | 1 | 6 | 1 | 0.14 | 0.07 |
| borrow | 10 | 0 | 10 | 0 | 10 | 0 | 0 | 0 | 0.00 | - |
| privacy | 5 | 0 | 5 | 4 | 1 | 4 | 0 | 4 | 0.80 | 1.00 |
| const-eval | 2 | 0 | 2 | 0 | 2 | 0 | 0 | 0 | 0.00 | - |
| cfg | 2 | 0 | 2 | 1 | 1 | 1 | 0 | 1 | 0.50 | 1.00 |
| macro-derive | 2 | 0 | 2 | 2 | 0 | 2 | 0 | 2 | 1.00 | 1.00 |
| macro-attr | 2 | 0 | 2 | 2 | 0 | 2 | 0 | 2 | 1.00 | 1.00 |
| macro-rules | 2 | 0 | 2 | 0 | 2 | 0 | 2 | 0 | 0.00 | 0.20 |
| macro-tokens | 3 | 0 | 3 | 2 | 1 | 2 | 1 | 2 | 0.67 | 0.75 |
| proc-macro-edit | 1 | 0 | 1 | 0 | 1 | 0 | 0 | 0 | 0.00 | - |
| valid | 5 | 0 | 0 | 0 | 0 | 5 | 0 | 5 | - | - |

### before: HEAD 1d5a2a6 (no unresolved-path detector), 2026-10-31, ra_ap 0.0.358, proc-macro on

| group | cases | skip | of | hit | FN | exact | FP | floor | recall | precision |
|---|---|---|---|---|---|---|---|---|---|---|
| unresolved-import | 7 | 0 | 7 | 4 | 3 | 3 | 2 | 4 | 0.57 | 0.80 |
| unresolved-name | 5 | 0 | 5 | 0 | 5 | 0 | 1 | 0 | 0.00 | - |
| unresolved-type | 2 | 0 | 2 | 0 | 2 | 0 | 1 | 0 | 0.00 | - |
| no-method | 10 | 0 | 10 | 10 | 0 | 9 | 1 | 10 | 1.00 | 0.91 |
| type-mismatch | 7 | 0 | 7 | 7 | 0 | 3 | 4 | 7 | 1.00 | 0.69 |
| trait-bound | 3 | 0 | 3 | 3 | 0 | 3 | 0 | 3 | 1.00 | 1.00 |
| field | 7 | 0 | 7 | 1 | 6 | 1 | 6 | 1 | 0.14 | 0.07 |
| borrow | 10 | 0 | 10 | 0 | 10 | 0 | 0 | 0 | 0.00 | - |
| privacy | 5 | 0 | 5 | 4 | 1 | 4 | 0 | 4 | 0.80 | 1.00 |
| const-eval | 2 | 0 | 2 | 0 | 2 | 0 | 0 | 0 | 0.00 | - |
| cfg | 2 | 0 | 2 | 1 | 1 | 1 | 0 | 1 | 0.50 | 1.00 |
| macro-derive | 2 | 0 | 2 | 2 | 0 | 2 | 0 | 2 | 1.00 | 1.00 |
| macro-attr | 2 | 0 | 2 | 2 | 0 | 2 | 0 | 2 | 1.00 | 1.00 |
| macro-rules | 2 | 0 | 2 | 0 | 2 | 0 | 2 | 0 | 0.00 | 0.20 |
| macro-tokens | 3 | 0 | 3 | 2 | 1 | 2 | 1 | 2 | 0.67 | 0.75 |
| proc-macro-edit | 1 | 0 | 1 | 0 | 1 | 0 | 0 | 0 | 0.00 | - |
| valid | 5 | 0 | 0 | 0 | 0 | 5 | 0 | 5 | - | - |

### before: HEAD 1d5a2a6 (no unresolved-path detector), proc-macro off

Floors are not checked with proc-macro off (the floor column is the on value).

| group | cases | skip | of | hit | FN | exact | FP | floor | recall | precision |
|---|---|---|---|---|---|---|---|---|---|---|
| unresolved-import | 7 | 0 | 7 | 4 | 3 | 3 | 2 | 4 | 0.57 | 0.80 |
| unresolved-name | 5 | 0 | 5 | 0 | 5 | 0 | 1 | 0 | 0.00 | - |
| unresolved-type | 2 | 0 | 2 | 0 | 2 | 0 | 1 | 0 | 0.00 | - |
| no-method | 10 | 0 | 10 | 10 | 0 | 9 | 1 | 10 | 1.00 | 0.91 |
| type-mismatch | 7 | 0 | 7 | 7 | 0 | 3 | 4 | 7 | 1.00 | 0.69 |
| trait-bound | 3 | 0 | 3 | 3 | 0 | 3 | 0 | 3 | 1.00 | 1.00 |
| field | 7 | 0 | 7 | 1 | 6 | 1 | 6 | 1 | 0.14 | 0.07 |
| borrow | 10 | 0 | 10 | 0 | 10 | 0 | 0 | 0 | 0.00 | - |
| privacy | 5 | 0 | 5 | 4 | 1 | 4 | 0 | 4 | 0.80 | 1.00 |
| const-eval | 2 | 0 | 2 | 0 | 2 | 0 | 0 | 0 | 0.00 | - |
| cfg | 2 | 0 | 2 | 1 | 1 | 1 | 0 | 1 | 0.50 | 1.00 |
| macro-derive | 2 | 0 | 2 | 0 | 2 | 0 | 0 | 2 | 0.00 | - |
| macro-attr | 2 | 0 | 2 | 1 | 1 | 1 | 1 | 2 | 0.50 | 0.50 |
| macro-rules | 2 | 0 | 2 | 0 | 2 | 0 | 2 | 0 | 0.00 | 0.20 |
| macro-tokens | 3 | 0 | 3 | 2 | 1 | 2 | 1 | 2 | 0.67 | 0.75 |
| proc-macro-edit | 1 | 0 | 1 | 0 | 1 | 0 | 0 | 0 | 0.00 | - |
| valid | 5 | 0 | 0 | 0 | 0 | 5 | 0 | 5 | - | - |

On and off differ only in macro-derive (hit 2 to 0) and macro-attr (hit 2 to
1); every other row is the same.

### before: HEAD 1d5a2a6 (no unresolved-path detector), other codes (proc-macro on; the same off)

Expected or reported codes outside the group of the case they appeared in.

| other code | missed | extra | cases |
|---|---|---|---|
| E0277 | 0 | 4 | 3 |
| E0282 | 0 | 2 | 2 |
| E0392 | 1 | 0 | 1 |
| E0405 | 1 | 0 | 1 |
| E0425 | 1 | 0 | 1 |
| E0433 | 4 | 0 | 1 |
| E0599 | 0 | 2 | 2 |

No case is MISGROUPED (every compared non-valid case expects at least one code
of its group). Measured at HEAD 7b13339, where Task B's baseline change (the
difference in the domain, counted per key) is already committed, with `git
status --short crates/ryo-v2-core crates/ryo-v2-ra` printing nothing: Task B
done. The older workspaces have no baseline errors, so B cannot move them; the
proc-macro-off macro-lab rows are the ones that depend on it.

### before: old definition (f11f5f1), proc-macro on

Old definition: pass = exact set equality; recall = pass / (pass + FN); a case
with only FP was in neither. Corpus then: 2 workspaces, 64 cases.

| group | cases | pass | FN | FP | skip | floor | recall |
|---|---|---|---|---|---|---|---|
| unresolved-import | 7 | 3 | 3 | 2 | 0 | 3 | 0.50 |
| unresolved-name | 5 | 0 | 5 | 1 | 0 | 0 | 0.00 |
| unresolved-type | 2 | 0 | 2 | 1 | 0 | 0 | 0.00 |
| no-method | 10 | 9 | 0 | 1 | 0 | 9 | 1.00 |
| type-mismatch | 7 | 3 | 0 | 4 | 0 | 3 | 1.00 |
| trait-bound | 3 | 3 | 0 | 0 | 0 | 3 | 1.00 |
| field | 7 | 1 | 6 | 6 | 0 | 1 | 0.14 |
| borrow | 10 | 0 | 10 | 0 | 0 | 0 | 0.00 |
| privacy | 5 | 4 | 1 | 0 | 0 | 4 | 0.80 |
| const-eval | 2 | 0 | 2 | 0 | 0 | 0 | 0.00 |
| cfg | 2 | 1 | 1 | 0 | 0 | 1 | 0.50 |
| valid | 4 | 4 | 0 | 0 | 0 | 4 | 1.00 |

### before: old definition (f11f5f1), proc-macro off

| group | cases | pass | FN | FP | skip | floor | recall |
|---|---|---|---|---|---|---|---|
| unresolved-import | 7 | 3 | 3 | 2 | 0 | 3 | 0.50 |
| unresolved-name | 5 | 0 | 5 | 1 | 0 | 0 | 0.00 |
| unresolved-type | 2 | 0 | 2 | 1 | 0 | 0 | 0.00 |
| no-method | 10 | 9 | 0 | 1 | 0 | 9 | 1.00 |
| type-mismatch | 7 | 3 | 0 | 4 | 0 | 3 | 1.00 |
| trait-bound | 3 | 3 | 0 | 0 | 0 | 3 | 1.00 |
| field | 7 | 1 | 6 | 6 | 0 | 1 | 0.14 |
| borrow | 10 | 0 | 10 | 0 | 0 | 0 | 0.00 |
| privacy | 5 | 4 | 1 | 0 | 0 | 4 | 0.80 |
| const-eval | 2 | 0 | 2 | 0 | 0 | 0 | 0.00 |
| cfg | 2 | 1 | 1 | 0 | 0 | 1 | 0.50 |
| valid | 4 | 4 | 0 | 0 | 0 | 4 | 1.00 |

On and off give the same outcome for all 64 cases (the corpus has no proc-macro
use).

H7 primary recall, new definition (hit / of over the five primary groups,
proc-macro on and off alike; the old definition gave 18/26 = 0.69):

1. Primary recall `hit / of`. Before the detector: 24/32 = 0.75 (hold band, 0.50
   <= 0.75 < 0.80): unresolved-import 4/7 = 0.57, unresolved-name 0/5 = 0.00,
   no-method 10/10, type-mismatch 7/7, trait-bound 3/3. With the detector:
   32/32 = 1.00 (adopt band, >= 0.80): unresolved-import 7/7, unresolved-name
   5/5, the other three unchanged.
2. Primary recall without G-B-3 and G-B-4 (cases whose group-code diagnostics
   are all tagged in `corpus/gap-tags.toml` leave the denominator). Before the
   detector: 24/24 = 1.00; the file tagged 20 expected diagnostics of
   primary-group cases (9 G-B-3, E0433; 11 G-B-4, E0425), each after reading
   its line in the overlay file, and the 8 cases they emptied were exactly the
   8 primary FN (bl remove-import-1, -2, typo-type-1, -2; et remove-import-1,
   -2, typo-type-1, -2, where bl is breakage-lab and et
   extract-trait-multicrate). With the detector: 32/32 = 1.00, equal to number 1
   because no tag is left (the detector found all 20). The tags that remain in
   the file are only diagnostics the detector misses.
3. All 8 primary FN were G-B-3 and G-B-4 (ra has no diagnostic for an
   unresolved type or path-prefix name), so no G-A row had to be fixed to move
   the recall; G-A-1 and G-A-2 only remove FP. Every primary group has at least
   3 compared cases with a group code, so no group's verdict is provisional.
   rustc 1.98.1 reports an unresolved type name as E0425 ("cannot find type"),
   not E0412, so such cases land in unresolved-name under the settled code
   table. The detector reports E0405 for a trait position, E0425 for any
   other type position and E0433 for the first segment of a path, so
   breakage-lab remove-import-0 (unresolved-type, not primary) lost its E0405
   and E0425 misses too.

Which definition H7 uses is the user's decision; this ledger does not choose.

### precision on history

False positives of the verifier on real commits. A pair is a commit (child) and
its first parent: the base is the parent's tree with the child's non-`.rs`
files (manifests, lock files), the overlay is the child's `.rs` files under
the workspace member dirs. Only pairs whose child builds cleanly under `cargo
check --workspace --all-targets` count (`valid`), so every error the verifier
reports on them is a false positive; a pair is one case, however many errors it
reports. A pair with a deleted or renamed `.rs` file, or an added or deleted
`Cargo.toml` at the root or in a member dir, is skipped (an overlay cannot delete
a file; the crate graph changed). A `.rs` change outside every member dir (the
fixture workspaces under `corpus/`) is dropped and counted, because ra does not
index it. Commands: `just history-export <repo> <range> <name> <out> <max>`
(git only), `just history-expect <out>/<name>`, `just oracle-history <out>`
(`<out>` has no `target` component). No floor: this is the first measurement.

| set | rev range | pairs | skip | valid | FP cases | FP rate | extra | unmapped |
|---|---|---|---|---|---|---|---|---|
| ryo-v2 | 9a0d816..1a6d103 | 5 | 0 | 5 | 0 | 0.00 | 0 | 0 |
| ryo-rs | 67b0ebfb~20..67b0ebfb | 20 | 4 | 16 | 0 | 0.00 | 0 | 0 |

ryo-rs here is ryo-rs (a verification repository used as a large sample).

ryo-rs (2026-10-31, large machine, cfg-test on, proc-macro on, unresolved-paths
on): the 4 skipped pairs change no member `.rs` file; all 16 valid pairs build
with 0 errors under `--all-targets` and the verifier reports nothing on any of
them. Per-pair check 0.5 to 19 s (median about 1.4 s; the slow ones touch
widely used crates). Together with ryo-v2: 0 of 21. 0 of 21 is compatible with
a true rate of up to about 0.14 at 95% (rule of three). Not yet measured: edits
of the kind an engine proposes (these are human commits).

ryo-v2: all 5 pairs are used, all with changed manifests (`manifest_from_child`);
overlay files 11, 14, 5, 9, 3; `.rs` changes dropped as outside the member dirs
59 (f11f5f1) and 13 (1d5a2a6), the corpus fixtures. 5 pairs give a wide error
(0 of 5 is compatible with a true rate of up to about 0.5 at 95%); read the
0.00 as "no false positive seen", not as a measured zero. The commits are
large (squashed milestones). Cost, measured: export 0.7 s and 9.9 MB; `expect` 89
s wall and 870 MB peak RSS for the 5 children (all build cleanly); the history
run 63 s and 1.66 GB peak for 1 pair and 232 s wall for the 5. ra's load builds
the proc-macros and build scripts of the dependency graph under `<base>/target`,
so the set grows to 1.6 GB after the run (9.9 MB before). ryo-rs (about 5 GB
per load) is a large run, outside the default test suite.

## Classes

- G-A: a gap of ryo-v2 (fix).
- G-B: a class rust-analyzer does not diagnose (E0412, borrow errors, ...). Not
  fixable here; the source of the rule catalog's "never applied automatically"
  list and of what commit-time CI must catch.
- G-C: outside the milestone (lints, `cfg(test)` code, build-script output).
- G-D: a false positive of rust-analyzer (type inference drift) that the
  baseline diff does not cancel.
- G-E: differs with the proc-macro server or build scripts on or off.
- G-H: the harness does not support the case (several workspaces, created or
  deleted files, ...).

## Gaps

| id | group | count | examples | root cause (concept) | decision | state |
|---|---|---|---|---|---|---|
| G-B-1 | borrow | 10 cases (E0382 2, E0499 2, E0502 2, E0505 2, E0506 2) | breakage-lab store/src/lib.rs:37 (use-after-move-0), :49 (double-mut-borrow-0), :63 (shared-then-mut-0), :75 (move-while-borrowed-0), :88 (assign-while-borrowed-0) | Diagnostic: ra's semantic diagnostics do not report MIR borrowck results (E0382/E0499/E0502/E0505/E0506) as Errors; ra reported nothing for any of the 10 | keep (outside automatic application; commit-time CI catches it) | open |
| G-B-2 | const-eval | 2 cases (E0080 2) | breakage-lab shapes/src/lib.rs:62 (const-eval-0), :63 (const-eval-1) | Diagnostic: ra does not report a failed const evaluation (overflow, out-of-bounds index) as an Error | keep (commit-time CI catches it) | open |
| G-B-5 | unresolved-type, unresolved-name | 4 cases, 11 diagnostics (E0422 11) | breakage-lab app/src/lib.rs:16,21,25,29,44 (remove-import-0), app/src/lib.rs:22,45 (typo-type-0, `Rectt { w, h }`); extract-trait-multicrate user/src/lib.rs:57,64,80 (remove-import-1), viewer/src/lib.rs:24 (remove-import-2), both in test modules and in group unresolved-name (V2 added them) | Diagnostic: ra reports nothing for an unresolved struct name in a struct literal (E0422). The E0405 of remove-import-0 (app/src/lib.rs:9, `impl<T: Area>`) is found by the detector | keep | open |
| G-B-6 | privacy | 1 case (E0603) | breakage-lab app/src/lib.rs:34 (call-private-2, `lab_shapes::scale` made private) | Diagnostic: ra reports no private-item error for a path into another crate (private methods, E0624, are reported: extract-trait-multicrate call-private-0..2 and broken-03 pass) | keep | open |
| G-B-7 | cfg | 1 case (E0425) | breakage-lab app/src/lib.rs:49 (cfg-gate-0, `lab_shapes::extra_only()` across crates). The same-crate bare call, shapes/src/lib.rs:75 (cfg-gate-1), is reported and passes. The same silence holds for a path to an item that does not exist in another crate: with cfg-test on ra reported nothing for `lab::missing_fn()` in cfg-test-lab `tests/it.rs` (E0425 missed once; that variant was measured while the case was built and replaced by a bare call, so it is not in the corpus) | Diagnostic: ra is silent on a path to a cfg-disabled item in another crate, and reports E0425 for the bare name in the same crate | keep | open |
| G-B-8 | field | 1 case (E0392) | breakage-lab app/src/lib.rs:6 (remove-field-0, `Wrapper<T>` with no field of type T). The code is in no group of the table | Diagnostic: ra does not report an unused type parameter | keep | open |
| G-A-1 | field | 6 cases, 21 diagnostics (E0609 missed, E0559 extra at the same lines) | breakage-lab shapes/src/lib.rs:24,30,42 and app/src/lib.rs:11; extract-trait-multicrate lib/src/lib.rs:25-37 (remove-field-0), :48-64 (remove-field-1, -2) | Diagnostic: ra's unresolved_field handler (ra_ap_ide_diagnostics 0.0.358, unresolved_field.rs:34) labels a field access as E0559; rustc says E0609 | fix: normalize in ryo-v2-ra (field access to E0609) | open |
| G-A-2 | type-mismatch, field, no-method, unresolved-import, unresolved-name | 13 cases, 20 diagnostics | breakage-lab app/src/lib.rs:34 (change-type-1 E0308), app/src/lib.rs:18,26,30 (drop-trait-impl-0), app/src/lib.rs:17,26,30,45 (remove-field-1 E0560), :22,45 (remove-field-2); extract-trait-multicrate user/src/lib.rs:20 (change-type-0), :34,36 (change-type-1), viewer/src/lib.rs:13 (change-type-2), viewer/src/lib.rs:10 (typo-type-0), user/src/lib.rs:49 (typo-type-1); V2 added the test modules of user: user/src/lib.rs:58 (remove-field-0 E0560), :66 (remove-field-1 E0560), :67 (remove-field-2 E0560), :81 (broken-06 E0063) | CrateGraph: cargo does not build a crate whose dependency failed, so rustc reports no errors there; ra reports them (true errors, but not in the oracle) | fix: drop errors of crates downstream of a crate with expected errors when comparing | open |
| G-D-1 | unresolved-type, unresolved-import, field, unresolved-name, cfg-test | 5 cases, 5 diagnostics | breakage-lab app/src/lib.rs:11 (remove-import-0 E0599), shapes/src/lib.rs:51 (remove-import-2 E0277), app/src/lib.rs:29 (remove-field-0 E0282); extract-trait-multicrate lib/src/lib.rs:146 (typo-type-1 E0282); cfg-test-lab lab/tests/it.rs:5 (cfgtest-it-1 E0425: ra reports the call of `helper` after the failed `use lab::helper`, rustc reports only the E0432) | Diagnostic: after an unresolved name rustc suppresses the follow-on error; ra infers on the unknown type and reports one | keep (a cascade; the baseline diff cannot cancel a new error) | open |
| G-D-2 | macro-rules, macro-tokens | 3 cases (rules-body-0 E0599, rules-body-1 E0308, tokens-rules-0 E0308): 3 missed, 5 extra diagnostics; on and off alike | macro-lab model/src/lib.rs:19 (rustc, inside the `getter!` definition) against :25 and :26 (ra, the two `getter!(..)` calls) in rules-body-0 and rules-body-1; tokens-rules-0 gets :26 from ra and :19 from rustc (its :54 in `listed` matches) | Diagnostic: for an error inside a `macro_rules!` expansion rustc's primary span is the definition token, while ra reports the macro call, once per call (2 calls give 2 diagnostics, rustc gives 1 distinct). G-D and not G-A because ra's diagnostic range is already the call site; whether ra exposes the definition span was not checked | keep (mapping it back would need ra's expansion info; not investigated) | open |
| G-E-1 | macro-derive | 2 cases (E0599 2), proc-macro off only | macro-lab model/src/lib.rs:29 (derive-removed-0), :34 (derive-removed-1) | Baseline: with the server off the derive is not expanded, so the baseline already holds an E0599 at each `describe()` call and removing the derive adds nothing new (inferred from "missed, no extra"; the baseline itself was not printed) | keep (the rule catalog must say that an edit touching a derive needs the proc-macro server) | open |
| G-E-2 | macro-attr | 1 case (E0308: 1 missed, 2 extra), proc-macro off only | macro-lab model/src/lib.rs:43 (attr-reparse-0: rustc and ra-on report the `#[reparse]` line) against :45 and :46 (ra-off, the lines as written); attr-keep-0 is the same on and off | CodeState: without the server the attribute is not applied, so the item keeps its own spans; with it, `#[reparse]` gives every token the call-site span, and ra-on follows | keep (use the server on) | open |
| G-E-3 | proc-macro-edit | 1 case (E0599 2 missed), on and off | macro-lab model/src/lib.rs:30,34 (pm-edit-0: the derive output renamed in macros/src/lib.rs) | CodeState: ra keeps the proc-macro dylib it built at load, so an edit of a proc-macro crate changes nothing the macros expand to and ra reports nothing | keep: an edit of a proc-macro crate is outside automatic application (ra keeps the load-time dylib); FN is the correct outcome. tests/oracle.rs fails if ra ever reports it | open |
| G-C-1 | cfg-test | 4 cases, 5 diagnostics (E0425 4, E0432 1): `cfgtest-mod-0` lab/src/lib.rs:24, `cfgtest-mod-1` lab/src/lib.rs:18, `cfgtest-it-1` lab/src/lib.rs:23 (E0425) and lab/tests/it.rs:1 (E0432), `cfgtest-it-0` lab/tests/it.rs:6; plus the 18 diagnostics V2 added to extract-trait-multicrate | cfg-test-lab; extract-trait-multicrate lib/src/lib.rs:179-218, user/src/lib.rs:57-80, viewer/src/lib.rs:24. `valid-02`'s overlay breaks the lib test module (E0599 lib/src/lib.rs:182), so that case is valid for lib and bins only and is in the cfg-test group | CodeState: the CI gate (`clippy --all-targets`, `cargo test`) compiles `#[cfg(test)]` code and `tests/` targets, but the oracle expected files were `cargo check --workspace` (lib and bins) and ra's `CargoConfig::set_test` was false, so ra diagnosed no `#[cfg(test)]` item (ra_ap_project_model 0.0.358, `workspace.rs` inserts the `test` cfg for local crates only when `set_test`; the earlier guess that ra enables it for members was wrong). Measured: hit 0 of 4 with cfg-test off, 4 of 4 with it on | **decided (V2)**: both sides see what CI sees: oracle truth = `cargo check --workspace --all-targets --keep-going`, the oracle runs ra with `RaConfig.cfg_test = true` (`RYO_V2_ORACLE_CFG_TEST=off` keeps the old setting, floors unchecked); `apply` loads with it too. The group floor `cfg-test` is 5 (4, then `valid-02` moved in). `just breakage-regroup` would move 5 cases and fail the floors unresolved-name 5 and type-mismatch 7 (hit 2 and 5, no case lost a hit); not applied; decided 2026-10-31 to keep the old groups (see Counts); the kept groups are recorded in `KEPT_GROUPS` and checked by `corpus_regroup_moves_nothing` | fixed |

Macro cases that match rustc exactly (no row): with the server on, the derive
(macro-derive 2/2), the attribute macros (macro-attr 2/2, `#[keep]` at the
edited line and `#[reparse]` at the call-site line) and the tokens of
`format!` and `vec!` (macro-tokens tokens-format-0, tokens-vec-0; also with
the server off). The counts of the rows above G-D-2 are unchanged by the new
recall definition (they count cases and diagnostics, not pass or hit).

## Fixed

| id | group | count | examples | root cause (concept) | decision | state |
|---|---|---|---|---|---|---|
| G-B-3 | unresolved-import, unresolved-name | 4 cases, 9 diagnostics (E0433) | breakage-lab shapes/src/lib.rs:40,41,50 (remove-import-2, `fmt::` path), app/src/lib.rs:38 (typo-type-1, `Binn::new`); extract-trait-multicrate user/src/lib.rs:29 (typo-type-2, `Case022::area`), user/src/lib.rs:28,31,32,35 (remove-import-1) | Diagnostic: ra reports nothing when the first segment of a path (`fmt::Display`, `Binn::new`) does not resolve | detector in ryo-v2-ra (unresolved.rs), RaConfig.unresolved_paths | fixed |
| G-B-4 | unresolved-name, unresolved-type | 6 cases, 12 diagnostics (E0425) | breakage-lab store/src/lib.rs:26 (remove-import-1), store/src/lib.rs:35 (typo-type-2, `b: Binn`), app/src/lib.rs:16 (remove-import-0); extract-trait-multicrate user/src/lib.rs:18,21,27,30,41,47 (remove-import-1), viewer/src/lib.rs:11,14 (remove-import-2), lib/src/lib.rs:129 (typo-type-1, `impl Case055`) | Diagnostic: ra reports nothing for an unresolved type name in a signature or `impl` head. rustc 1.98.1 gives this E0425 ("cannot find type"), not E0412, so the cases are grouped as unresolved-name | detector in ryo-v2-ra (unresolved.rs), RaConfig.unresolved_paths | fixed |

The detector skips a name some `use` of the file introduces (rustc reports only
the failed import), cfg-disabled code, macro input, and files with a glob import
ra cannot resolve. It is switched off with `RaConfig.unresolved_paths = false`
(oracle: `RYO_V2_ORACLE_UNRESOLVED=off`).
