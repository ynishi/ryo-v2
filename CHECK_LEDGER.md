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

Corpus: 3 workspaces, `base/extract-trait-multicrate` (8 hand cases + 18
breakage cases), `base/breakage-lab` (38 breakage cases) and `base/macro-lab`
(11 hand cases: a local proc-macro crate, a derive, two attribute macros and a
`macro_rules!`), 75 cases in all (`just oracle`; expected from `just
oracle-refresh`; breakage cases from `just breakage-gen`, groups from `just
breakage-regroup`). Groups with no case (arity) are omitted.

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
4 to 5 with the new macro-valid-0; the five macro groups are new).

### after: 2026-10-31, ra_ap 0.0.358, proc-macro on

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

### after: 2026-10-31, ra_ap 0.0.358, proc-macro off

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

### after: other codes (proc-macro on; the same off)

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

1. New definition: 24/32 = 0.75. By primary group: unresolved-import 4/7 =
   0.57, unresolved-name 0/5 = 0.00, no-method 10/10 = 1.00, type-mismatch 7/7
   = 1.00, trait-bound 3/3 = 1.00. It falls in the hold band (0.50 <= 0.75 <
   0.80): neither adopted nor discarded.
2. New definition with G-B-3 and G-B-4 excluded: 24/24 = 1.00. `corpus/gap-tags.toml`
   tags 20 expected diagnostics of primary-group cases (9 G-B-3, E0433; 11
   G-B-4, E0425), each tagged after reading its line in the overlay file; the 8
   cases whose group-code diagnostics are all tagged leave the denominator
   (bl remove-import-1, -2, typo-type-1, -2; et remove-import-1, -2, typo-type-1,
   -2, where bl is breakage-lab and et extract-trait-multicrate). They are
   exactly the 8 primary FN, so the remaining 24 cases all hit. It falls in the
   adopt band (>= 0.80). The number is only as good as the tags; the E0425 in
   breakage-lab remove-import-0 (unresolved-type, not primary) and the one in
   cfg-gate-0 (cfg, a value) are not tagged.
3. All 8 primary FN are G-B-3 and G-B-4 (ra has no diagnostic for an
   unresolved type or path-prefix name), so no G-A row has to be fixed to move
   the recall; G-A-1 and G-A-2 only remove FP. Every primary group has at least
   3 compared cases with a group code, so no group's verdict is provisional.
   rustc 1.98.1 reports an unresolved type name as E0425 ("cannot find type"),
   not E0412, so such cases land in unresolved-name under the settled code
   table.

Which definition H7 uses is the user's decision; this ledger does not choose.

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
| G-D-2 | macro-rules, macro-tokens | 3 cases (rules-body-0 E0599, rules-body-1 E0308, tokens-rules-0 E0308): 3 missed, 5 extra diagnostics; on and off alike | macro-lab model/src/lib.rs:19 (rustc, inside the `getter!` definition) against :25 and :26 (ra, the two `getter!(..)` calls) in rules-body-0 and rules-body-1; tokens-rules-0 gets :26 from ra and :19 from rustc (its :54 in `listed` matches) | Diagnostic: for an error inside a `macro_rules!` expansion rustc's primary span is the definition token, while ra reports the macro call, once per call (2 calls give 2 diagnostics, rustc gives 1 distinct). G-D and not G-A because ra's diagnostic range is already the call site; whether ra exposes the definition span was not checked | keep (mapping it back would need ra's expansion info; not investigated) | open |
| G-E-1 | macro-derive | 2 cases (E0599 2), proc-macro off only | macro-lab model/src/lib.rs:29 (derive-removed-0), :34 (derive-removed-1) | Baseline: with the server off the derive is not expanded, so the baseline already holds an E0599 at each `describe()` call and removing the derive adds nothing new (inferred from "missed, no extra"; the baseline itself was not printed) | keep (the rule catalog must say that an edit touching a derive needs the proc-macro server) | open |
| G-E-2 | macro-attr | 1 case (E0308: 1 missed, 2 extra), proc-macro off only | macro-lab model/src/lib.rs:43 (attr-reparse-0: rustc and ra-on report the `#[reparse]` line) against :45 and :46 (ra-off, the lines as written); attr-keep-0 is the same on and off | CodeState: without the server the attribute is not applied, so the item keeps its own spans; with it, `#[reparse]` gives every token the call-site span, and ra-on follows | keep (use the server on) | open |
| G-E-3 | proc-macro-edit | 1 case (E0599 2 missed), on and off | macro-lab model/src/lib.rs:30,34 (pm-edit-0: the derive output renamed in macros/src/lib.rs) | CodeState: ra keeps the proc-macro dylib it built at load, so an edit of a proc-macro crate changes nothing the macros expand to and ra reports nothing | keep: an edit of a proc-macro crate is outside automatic application (ra keeps the load-time dylib); FN is the correct outcome. tests/oracle.rs fails if ra ever reports it | open |

Macro cases that match rustc exactly (no row): with the server on, the derive
(macro-derive 2/2), the attribute macros (macro-attr 2/2, `#[keep]` at the
edited line and `#[reparse]` at the call-site line) and the tokens of
`format!` and `vec!` (macro-tokens tokens-format-0, tokens-vec-0; also with
the server off). The counts of the rows above G-D-2 are unchanged by the new
recall definition (they count cases and diagnostics, not pass or hit).
