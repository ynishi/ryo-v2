# Check ledger

The gaps between ryo-v2's in-memory verification (`ryo-v2-ra`, rust-analyzer HIR
diagnostics) and the oracle (`cargo check` run once, kept under
`corpus/<ws>/expected/`), as the oracle harness (`crates/ryo-v2-oracle`,
`tests/oracle.rs`) finds them. One row is one kind of gap with its count and
examples; the root cause names the domain concept that is inconsistent
(CodeState, Edit, Scope, Baseline, CrateGraph, Diagnostic, Verdict), not the
symptom. A floor is raised, never lowered without a row here.

Run: `just oracle` (`RYO_V2_ORACLE_REPORT=<file>` writes every outcome).

## Counts

Corpus: 2 workspaces, `base/extract-trait-multicrate` (8 hand cases + 18
breakage cases) and `base/breakage-lab` (38 breakage cases), 64 cases in all
(`just oracle`; expected from `just oracle-refresh`; breakage cases from `just
breakage-gen`, groups from `just breakage-regroup`). Groups with no case
(arity) are omitted. recall = pass / (pass + FN).

### 2026-10-31, ra_ap 0.0.358, proc-macro on

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

### proc-macro off

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

H7 primary recall: 18/26 = 0.69 (on and off). By primary group:
unresolved-import 3/6 = 0.50, unresolved-name 0/5 = 0.00, no-method 9/9 =
1.00, type-mismatch 3/3 = 1.00, trait-bound 3/3 = 1.00. Verdict: held
(0.50 <= 0.69 < 0.80), not adopted and not discarded. All 8 primary FN are
G-B-3 and G-B-4 (ra has no diagnostic for an unresolved type or path-prefix
name), so no G-A row has to be fixed to move the recall; G-A-1 and G-A-2 only
remove FP (and make the field group pass). The recall is decided by whether
those two G-B rows stay in the primary groups. Every primary group has at least
2 compared cases, so no group's verdict is provisional. rustc 1.98.1 reports
an unresolved type name as E0425 ("cannot find type"), not E0412, so such
cases land in unresolved-name under the settled code table.

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
| G-B-3 | unresolved-import, unresolved-name | 4 cases, 9 diagnostics (E0433) | breakage-lab shapes/src/lib.rs:40,41,50 (remove-import-2, `fmt::` path), app/src/lib.rs:38 (typo-type-1, `Binn::new`); extract-trait-multicrate user/src/lib.rs:29 (typo-type-2, `Case022::area`), user/src/lib.rs:28,31,32,35 (remove-import-1) | Diagnostic: ra reports nothing when the first segment of a path (`fmt::Display`, `Binn::new`) does not resolve | keep (not applied automatically; commit-time CI catches it) | open |
| G-B-4 | unresolved-name, unresolved-type | 6 cases, 12 diagnostics (E0425) | breakage-lab store/src/lib.rs:26 (remove-import-1), store/src/lib.rs:35 (typo-type-2, `b: Binn`), app/src/lib.rs:16 (remove-import-0); extract-trait-multicrate user/src/lib.rs:18,21,27,30,41,47 (remove-import-1), viewer/src/lib.rs:11,14 (remove-import-2), lib/src/lib.rs:129 (typo-type-1, `impl Case055`) | Diagnostic: ra reports nothing for an unresolved type name in a signature or `impl` head. rustc 1.98.1 gives this E0425 ("cannot find type"), not E0412, so the cases are grouped as unresolved-name | keep (same class as E0412 in the G-B definition) | open |
| G-B-5 | unresolved-type | 2 cases, 8 diagnostics (E0422 7, E0405 1) | breakage-lab app/src/lib.rs:16,21,25,29,44 and :9 (remove-import-0), app/src/lib.rs:22,45 (typo-type-0, `Rectt { w, h }`) | Diagnostic: ra reports nothing for an unresolved struct name in a struct literal (E0422) or a trait name in a bound (E0405) | keep | open |
| G-B-6 | privacy | 1 case (E0603) | breakage-lab app/src/lib.rs:34 (call-private-2, `lab_shapes::scale` made private) | Diagnostic: ra reports no private-item error for a path into another crate (private methods, E0624, are reported: extract-trait-multicrate call-private-0..2 and broken-03 pass) | keep | open |
| G-B-7 | cfg | 1 case (E0425) | breakage-lab app/src/lib.rs:49 (cfg-gate-0, `lab_shapes::extra_only()` across crates). The same-crate bare call, shapes/src/lib.rs:75 (cfg-gate-1), is reported and passes | Diagnostic: ra is silent on a path to a cfg-disabled item in another crate, and reports E0425 for the bare name in the same crate | keep | open |
| G-B-8 | field | 1 case (E0392) | breakage-lab app/src/lib.rs:6 (remove-field-0, `Wrapper<T>` with no field of type T). The code is in no group of the table | Diagnostic: ra does not report an unused type parameter | keep | open |
| G-A-1 | field | 6 cases, 21 diagnostics (E0609 missed, E0559 extra at the same lines) | breakage-lab shapes/src/lib.rs:24,30,42 and app/src/lib.rs:11; extract-trait-multicrate lib/src/lib.rs:25-37 (remove-field-0), :48-64 (remove-field-1, -2) | Diagnostic: ra's unresolved_field handler (ra_ap_ide_diagnostics 0.0.358, unresolved_field.rs:34) labels a field access as E0559; rustc says E0609 | fix: normalize in ryo-v2-ra (field access to E0609) | open |
| G-A-2 | type-mismatch, field, no-method, unresolved-import, unresolved-name | 9 cases, 16 diagnostics | breakage-lab app/src/lib.rs:34 (change-type-1 E0308), app/src/lib.rs:18,26,30 (drop-trait-impl-0), app/src/lib.rs:17,26,30,45 (remove-field-1 E0560), :22,45 (remove-field-2); extract-trait-multicrate user/src/lib.rs:20 (change-type-0), :34,36 (change-type-1), viewer/src/lib.rs:13 (change-type-2), viewer/src/lib.rs:10 (typo-type-0), user/src/lib.rs:49 (typo-type-1) | CrateGraph: cargo does not build a crate whose dependency failed, so rustc reports no errors there; ra reports them (true errors, but not in the oracle) | fix: drop errors of crates downstream of a crate with expected errors when comparing | open |
| G-D-1 | unresolved-type, unresolved-import, field, unresolved-name | 4 cases, 4 diagnostics | breakage-lab app/src/lib.rs:11 (remove-import-0 E0599), shapes/src/lib.rs:51 (remove-import-2 E0277), app/src/lib.rs:29 (remove-field-0 E0282); extract-trait-multicrate lib/src/lib.rs:146 (typo-type-1 E0282) | Diagnostic: after an unresolved name rustc suppresses the follow-on error; ra infers on the unknown type and reports one | keep (a cascade; the baseline diff cannot cancel a new error) | open |
