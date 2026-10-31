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

Corpus: `base/extract-trait-multicrate`, 8 hand cases (`just oracle`; expected
from `just oracle-refresh`). Groups with no case are omitted.

### 2026-10-31, ra_ap 0.0.358, proc-macro on

| group | cases | pass | FN | FP | skip | floor |
|---|---|---|---|---|---|---|
| unresolved-import | 2 | 2 | 0 | 0 | 0 | 2 |
| no-method | 2 | 2 | 0 | 0 | 0 | 2 |
| type-mismatch | 1 | 1 | 0 | 0 | 0 | 1 |
| field | 1 | 1 | 0 | 0 | 0 | 1 |
| privacy | 1 | 1 | 0 | 0 | 0 | 1 |
| valid | 1 | 1 | 0 | 0 | 0 | 1 |

### proc-macro off

| group | cases | pass | FN | FP | skip | floor |
|---|---|---|---|---|---|---|
| unresolved-import | 2 | 2 | 0 | 0 | 0 | 2 |
| no-method | 2 | 2 | 0 | 0 | 0 | 2 |
| type-mismatch | 1 | 1 | 0 | 0 | 0 | 1 |
| field | 1 | 1 | 0 | 0 | 0 | 1 |
| privacy | 1 | 1 | 0 | 0 | 0 | 1 |
| valid | 1 | 1 | 0 | 0 | 0 | 1 |

H7 primary recall: 5/5 = 1.00 (on and off). N is small (5 cases over 3 of the 5
primary groups; unresolved-name and trait-bound have none), so the adopt /
discard verdict is provisional.

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
