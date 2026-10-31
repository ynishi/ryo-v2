# Search ledger

How often the search (`search_decision`, M4) keeps the best change of a decision when it prunes to
a beam: H6, measured by the harness `search-recall` (`crates/ryo-v2-oracle/src/search_recall.rs`).
The truth is a planted corpus (`corpus/search`): clean code with the inverse of one or two
refactors injected, so the best change is the one that restores the clean text, known by
construction and independent of every metric (`crates/ryo-v2-oracle/src/planted.rs`). The ledger
holds numbers of runs that were done, nothing projected; a row marked pending has not been run.

Run: `just search-recall` (the small bases, about 15 s). Cases are written by
`just search-recall-gen <ws> <seeds>` and committed; the generator writes `probes = []` and the
maintainer fills the probes by hand (until M5, which changes the extensibility metric, not the
cases). The committed small-base cases carry probes since 2026-10-31 (section "Probes").

## Definitions

One decision is one planted case: the workspace, the planted edits, the touched functions and
items as targets, no root Blueprints, and the depth of the best (1 for one plant, 2 for two plants
in different functions). The search starts from the planted state and runs `max_depth = depth`.

- **best**: the state whose files equal the clean files up to whitespace runs (and, for a folded
  helper, up to the name the expander gave the extracted function). A state with no edit left is
  the base, which is clean.
- **generated**: some child at the depth of the case is the best. This measures the expander, not
  the pruning.
- **hit**: such a child survived the pruning (it was checked, or audited anyway).
- **recall@k** = hit / generated, over decisions whose best was generated; k is the beam (20 by
  default). Reported beside it, never folded in: **hit/all** (hit over every measured decision),
  the **generation** rate (generated over measured) and **top-3** (a clean state is among the
  first three of the final ranking, over measured).
- **beam rank**: the place of the best in the beam (from 0). **final rank**: its place in the
  ranking of every checked node (from 0). **dropped at**: the stage that removed a generated best
  (`dominated`, `rank`, `noul`, `duplicate`, or an M3 stage).
- A planted state that does not verify is skipped, not counted.

Adopt H6 when recall@20 >= 0.8, discard when < 0.5, hold between; fewer than 30 compared
(= generated) decisions decide nothing (`insufficient`). With 0 misses in 30 the true miss rate is
at most 0.1 at 95 % (rule of three). Thresholds are estimates fixed before the first run.

## Decisions in force

The decisions of the search that outlive a change, one row each, with the measurement that settled
it and the guard that fails when the value is broken. All were accepted 2026-10-31. A row is
changed, with a new measurement, when its "revisit when" is met; a rejected option is not taken up
again without a new measurement. The measurements are from `docs/bench-m2.md` (assert off,
Affected scope, warm session; 12 threads, 30 GiB). ryo-rs is a verification repository used as a
large sample.

| id | decision (current value) | why it holds today (measurement, date) | guard (constant / test / contract clause) | revisit when | rejected options |
|---|---|---|---|---|---|
| D1 time budget of one decision | One decision is one `search_decision` call (expand, prune, verify the survivors with `Verifier::check`, repeat to a depth, rank). Wall time 300 s soft and 360 s hard; 360 s = 20 x 3 x 6 s, the product of `beam`, `max_depth` and `per_candidate_ms` of `DEFAULT_SEARCH_BUDGET`. Soft: no new depth starts once 300 s have passed (`SearchReport.soft_stopped`); hard: the search stops at once (`SearchReport.budget_exhausted`). One check is cut off after 6 s (`per_candidate_ms`, `CheckError::TimedOut`); the candidate is recorded as `Blocking::BudgetExhausted { limit_ms }` and the search goes on. Expand plus static metrics: 10 s per depth (200 candidates x 50 ms). Judge: one call per depth, 30 s; the metric judge takes 0. At the hard limit the result is `Decided` with what has been verified so far; with nothing passed it is `Failed` with `budget_exhausted: true`. The expand and judge allowances are recorded, not enforced, because the metric judge and the syntactic assessor are far below them. Wall time is read from the `Clock` port, so tests drive the budget with a fake clock. | 2026-10-31, `docs/bench-m2.md`: one check takes 0.3 to 4.3 s, one revert 0.04 to 0.36 s, check plus revert 2 to 3 s at the median. At most 20 candidates per depth over at most 3 depths is at most 60 checks, which is 120 to 180 s at the median, so 300 s leaves the second half of the budget for expansion, the judge and the slow tail, and 360 s is derived from the knobs, not chosen separately. 6 s is a little above the measured 4.3 s maximum; a check beyond it is a candidate Vision principle 15 (5 s or less per candidate) already rejects. A caller who waits 6 minutes gets the verified candidates, not an error. Consequence: a base where most checks exceed 6 s yields `Failed`, the signal to start the perf work of D4. | `DEFAULT_SEARCH_BUDGET` (beam 20, `max_expand` 200, `beam_parents` 4, `max_depth` 3, `per_candidate_ms` 6,000, `soft_ms` 300,000, `hard_ms` 360,000), `EXPAND_DEPTH_MS` = 10,000, `JUDGE_DEPTH_MS` = 30,000; contract clause C1 of `DEFAULT_SEARCH_BUDGET`, test `app::search::tests::default_search_budget_is_the_decided_numbers` (pins every value, including the allowances); behaviour: `soft_budget_stops_after_the_depth`, `budget_exhausted_returns_verified_so_far`, `hard_budget_with_nothing_verified_is_failed_exhausted`, `check_timeout_is_budget_exhausted_blocking` (core), `tests/timeout.rs check_past_timeout_is_timed_out_and_session_usable` (ra). The allowances have no behaviour guard: they are not enforced. | The median decision on the ryo-rs corpus exceeds 300 s (D4, trigger 1); the measured slowest check exceeds 6 s on a base the user cares about; a real judge (an LLM) is added, whose 30 s per depth becomes an enforced limit. | No budget (the M3 state): one decision's duration cannot be stated, so principle 15 cannot be checked at all. A candidate-count budget only (beam times depth): on a large base one check takes more than 4 s as a matter of course, so the same count means 2 minutes or 10 and the time cannot be read from it. |
| D2 revert assertion in production | `Verifier::check` applies edits to a session, diagnoses and reverts. The text fingerprint comparison of the touched files runs on every check (cheap, O(touched bytes)). The diagnostic re-assert (diagnose the touched files again and compare with the baseline) runs on every 50th check (`RaConfig.assert_revert_every = Some(50)`) and once at the end of every decision, before the Resolution is returned (`RevertAudit::audit_revert`). The bench and the oracle keep `Some(1)`. A mismatch poisons the session (`CheckError::RevertMismatch`, then `Poisoned`); the caller reloads it, which costs about 40 s (warm load 3.2 s plus baseline 35 s), outside the budget of D1 and recorded as evidence. A mismatch found at the audit rejects the whole decision (`SearchError::RevertAudit`), not only the last check, because it cannot say when the drift began. `RevertAudit` is a port beside `Verifier`; the fake verifier of the core tests implements it. | 2026-10-31, `docs/bench-m2.md`: the re-assert costs 1.4 to 2.1 s, which doubles the cost of a check, so every check cannot pay it in a search of 60. Every 50 checks it is 30 to 40 ms per check, 1 to 2 % of a check. The end-of-decision audit costs one re-assert (about 2 s of 360 s) and means a `Decided` result has always been followed by a clean diagnostic comparison of the session it came from. The fingerprint catches text drift (review finding V5) on every check; what it cannot see is the analyzer's derived state drifting while the text is right, and sampling plus the final audit bounds how long such a drift can go unnoticed. | `ASSERT_REVERT_EVERY` = 50, `RaConfig.assert_revert_every`; contract clauses C1 to C3 of `RaAnalyzer::check` and C1, C2 of `RevertAudit for RaAnalyzer`; tests `tests/verifier.rs check_leaves_text_and_baseline_at_base`, `tests/verifier.rs assert_revert_every_samples_the_diagnostic_assert`, `tests/timeout.rs audit_revert_is_ok_after_checks_and_refuses_poisoned`, core `revert_audit_runs_once_at_end`; the value 50 is pinned by `default_search_budget_is_the_decided_numbers`. The sampling period is a constant; changing it needs a new bench of the poison rate. | A poison (revert mismatch) is seen in production use: lower the period or audit every depth; the re-assert gets cheaper (item-level scope, D4 order step 1), which allows a shorter period; the check time grows by 1.5 times over a long run (D4, trigger 3). | Re-assert on every check: the cost of a check doubles (the 3 to 6 s of the bench) and the 6 s cut-off of D1 would fire on healthy checks. Fingerprint only: if salsa state drifts without the text changing nothing detects it and `Decided` is silently wrong, which is the failure the engine exists to avoid. |
| D3 parallelism and memory | One decision uses one session (one loaded rust-analyzer database) and verifies its candidates one after another, siblings consecutively. The only parallelism is inside a check: the diagnostic threads (`RaConfig.threads`, default the available parallelism) split the files of the scope. More sessions are started only after a D4 trigger fires, and then at most `N = max(1, floor((RAM_GB - 8) / 6))` (`RESERVED_RAM_GB = 8`, `SESSION_RAM_GB = 6`; N = 3 on 30 GB), stated by `max_sessions(ram_gb)` in `ryo-v2-core`. The `Verifier` port keeps `&mut Session`: one check at a time per session, enforced by the type. A caller that wants two decisions at once starts two processes and owns the memory budget. `max_sessions` exists as a function and a test, not as a feature. | 2026-10-31, `docs/bench-m2.md` (30 GiB, 12 threads): one session of the base of ryo-rs takes about 5 GB and 60 repeated runs grew it by about 0.75 GB, so 6 GB per session; a second `AnalysisHost` doubles the resident set size; salsa gives no writable fork, so edits are applied to the one database and reverted. The diagnostics of one check already run on all cores by file, so a second session adds parallelism only for the cores one check leaves idle (an Amdahl bound, not measured). The memory cap decides first: 8 GB stay free for the rest of the machine, so 3 sessions on 30 GB and a best case of 3 times. Serial checks keep the search deterministic and the revert audit simple (one session, one history); races and poisoning with several sessions are a class of failure to take on only when the serial path is correct and too slow. | `max_sessions`, `SESSION_RAM_GB` = 6, `RESERVED_RAM_GB` = 8; contract clause C1 of `max_sessions`, test `app::search::tests::max_sessions_follows_decision_d3` (30 gives 3, 0 and 8 and 14 give 1, 20 gives 2, 64 gives 9); the two constants are pinned by `default_search_budget_is_the_decided_numbers`; the `&mut Session` of the `Verifier` port is enforced by the type. | A D4 trigger fires (the median decision exceeds 300 s, or the median warm check exceeds 5 s) after the cheaper steps of its order are done; the backend offers a cheap, writable fork; the machine has less than 14 GB (the formula gives 1; one session needs 6 GB to be safe). | N sessions now: the memory caps the gain at 3 times on a 30 GB machine, and the race and poison handling would be built before the serial path is shown correct and too slow. Several forks in one session: the backend (salsa) has no writable fork, so this is not buildable without replacing the backend. |
| D4 when to start perf work | No perf work during M4; the harness only measures. A perf task is opened when any of these happens on the ryo-rs decision corpus (the Perf triggers section below shows their status): (1) the median wall time of a decision exceeds 300 s (the soft limit of D1); (2) the median warm check in Affected scope exceeds 5 s (Vision principle 15); (3) after 1,000 checks the resident set size exceeds 8 GB, or the check time has grown by 1.5 times (review finding V4); (4) one recall run of the planted corpus takes more than 10 minutes in CI. Work order when a trigger fires, cheapest and most effective first: (a) item-level scope: diagnose only the files that reference the changed item (434 of 504 files today; tens expected, an estimate), with the missed-error risk measured by the oracle; (b) order siblings so that checks that touch the same files run together; (c) parent-to-child deltas inside the adapter (skip reverting to the base between a parent and its child); (d) N sessions (D3). Each step follows the usual profiling procedure: save a baseline, take a flame graph, change the hot path only, bench again. The constants live in `ryo-v2-core::app::search` so the harness and this row cannot disagree. The ryo-rs run is outside the default test suite (more than 10 minutes), so trigger 4 is judged from the small bases and the wall time of the large run together. | 2026-10-31: the numbers behind D1 to D3 come from small and medium bases and the decision corpus of ryo-rs does not exist yet at scale; perf work started without a number to move is guesswork, and perf work never started leaves the budget of D1 unwatched. Item-level scope attacks the largest factor (files diagnosed per check); deltas only remove the revert, which is 0.04 to 0.36 s of a 0.3 to 4.3 s check, about 10 %. The triggers are the limits D1 and principle 15 state, so each says "a promise we made is broken" and not "something could be faster". | Trigger 1: `TRIGGER_DECISION_MEDIAN_MS` = 300,000. Trigger 2: `TRIGGER_CHECK_MEDIAN_MS` = 5,000. Trigger 3, memory: `TRIGGER_RSS_KB` = 8 x 1024 x 1024 and `TRIGGER_AFTER_CHECKS` = 1,000; trigger 3, growth: `TRIGGER_CHECK_GROWTH_PERMILLE` = 1,500 against the median of the last `search_recall::GROWTH_WINDOW` = 100 checks in thousandths of the median of the first 100, read after `TRIGGER_AFTER_CHECKS` checks by `search_recall::check_growth_permille` through `trigger_lines`, test `trigger_3_reads_the_growth_limit` (the limit itself does not fire, one ms above it does, one check fewer is not measured) and `check_growth_compares_the_ends_of_the_run`; the checks of all measured decisions are flattened in row order, so on a corpus of several workspaces (one session each) the two windows can come from different sessions: the growth half is meant for the one-workspace ryo-rs run. Trigger 4: `TRIGGER_HARNESS_MS` = 600,000. Values pinned by `default_search_budget_is_the_decided_numbers`; `search_recall::trigger_lines` prints triggers 1, 2, 3 (memory and growth) and 4, pinned by test `triggers_follow_the_decided_numbers`. | The first full ryo-rs run is in this ledger: compare each trigger with its number; a trigger fires (this row is then superseded by the perf task's own record); a base larger than ryo-rs is the target. | Optimise now (parent-to-child deltas, N sessions): deltas save about 10 % of a check and add a second state to verify (the child's state on top of the parent's), a new way to be wrong. Ignore perf and go on to M5: nobody would notice when the budget of D1 stops being met. |

## Counts

Corpus and runs of the small bases: 2026-10-31, repo HEAD 64dcbfb plus the uncommitted M4 work (T1 to T4), rustc
1.98.1, ra_ap 0.0.358, 12 threads, 30 GiB, proc-macro on,
`RaConfig.assert_revert_every = 50` (decision D2), `MetricJudge`, `SyntaxAssessor`, Affected scope,
one session per workspace, no probes.

| corpus | decisions | depth 2 | skipped | generated | hit | recall@20 | generation | status |
|---|---|---|---|---|---|---|---|---|
| small bases (below) | 39 | 10 | 0 | 31 | 30 | 0.97 | 0.79 | verdict adopt, 31 compared |
| ryo-rs | 11 | 2 | 0 | 6 | 6 | 1.00 | 0.55 | verdict insufficient, 6 compared |

ryo-rs here is ryo-rs (a verification repository used as a large sample); its run, with the
machine it ran on, is in section "ryo-rs (large run)".

The small bases are tiny (the four `corpus/base` workspaces hold 691 lines in all) and give almost
no site; most decisions come from `search-lab`, a hand-written workspace of two crates (`geo`: 4
files, `app`: 2 files, 453 lines, rustfmt-formatted, compiles clean with and without the
planted edits) written to hold sites of every kind.

| workspace | decisions | notes |
|---|---|---|
| `search/base/search-lab` | 38 | 28 at depth 1 (7 obscure-name, 9 inline-variable, 3 inline-function, 9 misplace-item) and 10 pairs |
| `base/breakage-lab` | 1 | one obscure-name site (`store/src/lib.rs`); no other kind has a site |
| `base/cfg-test-lab`, `base/macro-lab`, `base/extract-trait-multicrate` | 0 | no site of any kind |

Run at beam 20 (`search-recall run corpus/search`, 14.6 s), without probes (every case had
`probes = []`; the run with probes is in section "Probes" and gives the same numbers):

| set | decisions | skipped | generated | hit | recall@20 | hit/all | generation | top-3 |
|---|---|---|---|---|---|---|---|---|
| all | 39 | 0 | 31 | 30 | 0.97 | 0.77 | 0.79 | 0.72 |
| ws breakage-lab | 1 | 0 | 1 | 1 | 1.00 | 1.00 | 1.00 | 1.00 |
| ws search-lab | 38 | 0 | 30 | 29 | 0.97 | 0.76 | 0.79 | 0.71 |
| kind inline-function | 6 | 0 | 5 | 4 | 0.80 | 0.67 | 0.83 | 0.33 |
| kind inline-variable | 14 | 0 | 5 | 5 | 1.00 | 0.36 | 0.36 | 0.36 |
| kind misplace-item | 14 | 0 | 13 | 13 | 1.00 | 0.93 | 0.93 | 0.86 |
| kind obscure-name | 15 | 0 | 15 | 14 | 0.93 | 0.93 | 1.00 | 0.93 |
| depth 1 | 29 | 0 | 24 | 24 | 1.00 | 0.83 | 0.83 | 0.79 |
| depth 2 | 10 | 0 | 7 | 6 | 0.86 | 0.60 | 0.70 | 0.50 |

A kind row counts the decisions that plant the kind, so a pair is in two rows.

Verdict: **adopt** on the numbers (recall@20 0.97 over 31 generated decisions), with two
reservations that the next sections spell out. 1 miss in 31 puts the true miss rate at most 0.14
at 95 % (exact binomial, one-sided); at depth 1, 0 misses in 24 puts it at most 0.12.

1. At depth 1 the beam never fills: the expander proposes 2 to 11 children, so recall@20 at
   depth 1 cannot be below 1.00. The pruning is exercised at depth 2, where 18 to 76 children are
   proposed and the beam is full in 9 of the 10 pairs, and by the narrow-beam runs below.
2. `search-lab` is small and written for this harness: its sites are natural to the generator.

## Probes

Since 2026-10-31 every committed case of the small bases names one to three expected spec changes,
the next change a maintainer of that code would plausibly make (case file field `probes`, kinds
`add-variant`, `add-field`, `add-method`). They are chosen from the types the planted code builds
or reads: a new shape (`add-variant` on `Shape`) where the code goes over shapes, a new field on
`Bounds`, `Rect`, `Line`, `Report`, `Slot` or `Store` where the code builds or reads one, and a
new field on `Bin` for the one breakage-lab case. Neither workspace defines a trait of its own (the
only `impl` of a trait is `Default for Store`), so no `add-method` probe is plausible and none is
used. The test `one_planted_case_runs_end_to_end` (`crates/ryo-v2-oracle/tests/search_recall.rs`)
checks that the probes of a case file reach the case, that every committed case has probes, and
that each probe names a type its workspace defines as the matching item (`enum` for add-variant,
`struct` for add-field, `trait` for add-method), so an add-method probe on these workspaces fails it.

| case kind (decisions) | probes | add-variant | add-field | per case |
|---|---|---|---|---|
| obscure-name, depth 1 (8, one in breakage-lab) | 10 | 5 | 5 | 1 to 2 |
| inline-variable, depth 1 (9) | 12 | 4 | 8 | 1 to 2 |
| inline-function, depth 1 (3) | 4 | 1 | 3 | 1 to 2 |
| misplace-item, depth 1 (9) | 11 | 2 | 9 | 1 to 2 |
| pairs, depth 2 (10) | 24 | 6 | 18 | 2 to 3 |
| all (39) | 61 | 18 | 43 | |

Run with probes, beam 20 (`search-recall run corpus/search`, 19.7 s, repo HEAD 747c3d8 plus the
probes), next to the run without probes above:

| set | without probes: generated / hit / recall@20 / top-3 | with probes: generated / hit / recall@20 / top-3 |
|---|---|---|
| all | 31 / 30 / 0.97 / 0.72 | 31 / 30 / 0.97 / 0.72 |
| ws breakage-lab | 1 / 1 / 1.00 / 1.00 | 1 / 1 / 1.00 / 1.00 |
| ws search-lab | 30 / 29 / 0.97 / 0.71 | 30 / 29 / 0.97 / 0.71 |
| kind inline-function | 5 / 4 / 0.80 / 0.33 | 5 / 4 / 0.80 / 0.33 |
| kind inline-variable | 5 / 5 / 1.00 / 0.36 | 5 / 5 / 1.00 / 0.36 |
| kind misplace-item | 13 / 13 / 1.00 / 0.86 | 13 / 13 / 1.00 / 0.86 |
| kind obscure-name | 15 / 14 / 0.93 / 0.93 | 15 / 14 / 0.93 / 0.93 |
| depth 1 | 24 / 24 / 1.00 / 0.79 | 24 / 24 / 1.00 / 0.79 |
| depth 2 | 7 / 6 / 0.86 / 0.50 | 7 / 6 / 0.86 / 0.50 |

The beam rank, fate, final rank, proposed and kept of each of the 39 decisions are the same as in
the table "Decisions (beam 20)" below; the one miss is the same decision, dropped `dominated`.
The rankings are the same where the order matters most as well: `--exhaustive` gives 31 of 31
over 879 checks with the same final ranks as section "Exhaustive" (the depth-2 obscure-name plus
inline-function best at beam rank 25, final rank 9), and `--beam 1` gives 23 of 31 (0.74, 8
`dominated`), inline-function 0/5, inline-variable 3/5, misplace-item 12/13, obscure-name 12/15,
the row of beam 1 under "Narrow beams".

Why nothing moved (from the definition, not a separate measurement; the report prints no
per-candidate extensibility): `SyntaxAssessor` counts, in the files a step touches, the matches
without a catch-all arm that name the probed enum, the record literals and patterns of the probed
struct without `..`, and the impls of the probed trait, and the extensibility change is the parent's
count minus the child's. The M4 expander proposes renames, extract and inline of a variable or a
function, and moves of an item between files; none of them adds or removes such a match, literal,
pattern or impl, it only renames or moves it, so the change is 0 for every candidate whatever the
probes are. The extensibility term is now fed, but it can only separate candidates once the
expander proposes an edit that changes a site count (a catch-all arm, a `..` rest, a literal folded
into a constructor, a match turned into a method), or once M5 replaces the metric.

## Decisions (beam 20)

| decision | kinds | depth | generated | hit | beam rank | fate | final rank | proposed | kept | wall ms |
|---|---|---|---|---|---|---|---|---|---|---|
| breakage-lab/obscure-name-0 | obscure-name | 1 | yes | yes | 3 | passed | 1 | 6 | 6 | 62 |
| search-lab/inline-function-0 | inline-function | 1 | yes | yes | 3 | passed | 3 | 10 | 10 | 48 |
| search-lab/inline-function-1 | inline-function | 1 | yes | yes | 2 | passed | 2 | 7 | 7 | 190 |
| search-lab/inline-function-2 | inline-function | 1 | yes | yes | 1 | passed | 1 | 9 | 9 | 172 |
| search-lab/inline-variable-0 | inline-variable | 1 | no | no | - | - | - | 7 | 7 | 40 |
| search-lab/inline-variable-1 | inline-variable | 1 | yes | yes | 2 | passed | 2 | 8 | 8 | 138 |
| search-lab/inline-variable-2 | inline-variable | 1 | yes | yes | 0 | passed | 0 | 6 | 6 | 111 |
| search-lab/inline-variable-3 | inline-variable | 1 | no | no | - | - | - | 3 | 3 | 19 |
| search-lab/inline-variable-4 | inline-variable | 1 | no | no | - | - | - | 7 | 7 | 116 |
| search-lab/inline-variable-5 | inline-variable | 1 | no | no | - | - | - | 6 | 6 | 102 |
| search-lab/inline-variable-6 | inline-variable | 1 | yes | yes | 0 | passed | 0 | 6 | 6 | 97 |
| search-lab/inline-variable-7 | inline-variable | 1 | yes | yes | 0 | passed | 0 | 7 | 7 | 111 |
| search-lab/inline-variable-8 | inline-variable | 1 | no | no | - | - | - | 6 | 6 | 104 |
| search-lab/misplace-item-0 | misplace-item | 1 | yes | yes | 0 | passed | 0 | 2 | 2 | 23 |
| search-lab/misplace-item-1 | misplace-item | 1 | yes | yes | 0 | passed | 0 | 4 | 4 | 131 |
| search-lab/misplace-item-2 | misplace-item | 1 | yes | yes | 0 | passed | 0 | 6 | 6 | 172 |
| search-lab/misplace-item-3 | misplace-item | 1 | yes | yes | 0 | passed | 0 | 6 | 6 | 50 |
| search-lab/misplace-item-4 | misplace-item | 1 | yes | yes | 0 | passed | 0 | 5 | 5 | 129 |
| search-lab/misplace-item-5 | misplace-item | 1 | yes | yes | 0 | passed | 0 | 4 | 4 | 107 |
| search-lab/misplace-item-6 | misplace-item | 1 | yes | yes | 0 | passed | 0 | 5 | 5 | 158 |
| search-lab/misplace-item-7 | misplace-item | 1 | yes | yes | 0 | passed | 0 | 6 | 6 | 170 |
| search-lab/misplace-item-8 | misplace-item | 1 | yes | yes | 0 | passed | 0 | 2 | 2 | 22 |
| search-lab/obscure-name-0 | obscure-name | 1 | yes | yes | 0 | passed | 0 | 9 | 9 | 49 |
| search-lab/obscure-name-1 | obscure-name | 1 | yes | yes | 0 | passed | 0 | 8 | 8 | 54 |
| search-lab/obscure-name-2 | obscure-name | 1 | yes | yes | 0 | passed | 0 | 6 | 6 | 39 |
| search-lab/obscure-name-3 | obscure-name | 1 | yes | yes | 0 | passed | 0 | 8 | 8 | 145 |
| search-lab/obscure-name-4 | obscure-name | 1 | yes | yes | 0 | passed | 0 | 8 | 8 | 141 |
| search-lab/obscure-name-5 | obscure-name | 1 | yes | yes | 0 | passed | 0 | 11 | 11 | 183 |
| search-lab/obscure-name-6 | obscure-name | 1 | yes | yes | 0 | passed | 0 | 11 | 11 | 194 |
| search-lab/pair-inline-function-misplace-item-0 | inline-function+misplace-item | 2 | yes | yes | 7 | passed | 3 | 16/57 | 16/20 | 291 |
| search-lab/pair-inline-variable-inline-function-0 | inline-variable+inline-function | 2 | no | no | - | - | - | 16/66 | 16/20 | 618 |
| search-lab/pair-inline-variable-inline-variable-0 | inline-variable+inline-variable | 2 | no | no | - | - | - | 13/54 | 13/20 | 394 |
| search-lab/pair-inline-variable-misplace-item-0 | inline-variable+misplace-item | 2 | no | no | - | - | - | 13/45 | 13/20 | 266 |
| search-lab/pair-misplace-item-misplace-item-0 | misplace-item+misplace-item | 2 | yes | yes | 0 | passed | 0 | 6/18 | 6/10 | 374 |
| search-lab/pair-obscure-name-inline-function-0 | obscure-name+inline-function | 2 | yes | no | - | dominated | - | 16/64 | 16/20 | 484 |
| search-lab/pair-obscure-name-inline-variable-0 | obscure-name+inline-variable | 2 | yes | yes | 2 | passed | 2 | 16/65 | 16/20 | 477 |
| search-lab/pair-obscure-name-misplace-item-0 | obscure-name+misplace-item | 2 | yes | yes | 0 | passed | 0 | 15/55 | 15/20 | 287 |
| search-lab/pair-obscure-name-obscure-name-0 | obscure-name+obscure-name | 2 | yes | yes | 0 | passed | 0 | 17/68 | 17/20 | 418 |
| search-lab/pair-obscure-name-obscure-name-1 | obscure-name+obscure-name | 2 | yes | yes | 0 | passed | 0 | 19/76 | 19/20 | 511 |

`proposed` and `kept` list one number per depth.

### Dropped at

| dropped at | decisions |
|---|---|
| dominated | 1 |

The one miss is a depth-2 best (`obscure-name` and `inline-function` planted) removed by the
dominance rule of the pruning before the rank cut. In the exhaustive run below the same best is
beam rank 25, final rank 9: it is not a bad candidate, it is a heavy one (the extract-function
restore writes more bytes).

### Generation (kept beside recall, not in it)

8 of 39 decisions did not generate the best. All 8 plant an `inline-variable` whose use is
inside a `let` initializer (5 at depth 1: `pack`, `load`, `summary`, `fits`, `run`; 3 pairs that
contain `run`). The M4 expander proposes extract-variable only for an expression that is not
inside the initializer of a `let` of a plain name (its probe: the same expression in an
expression statement, a compound assignment, a tail expression or a block is proposed, in a `let`
it never is). Every `inline-variable` plant whose use is an expression statement or a tail
expression was generated (5 of 5). This is a gap of the expander, found by the harness, not a
pruning loss: its tests did not cover the `let` case.

## Narrow beams (the pruning, stressed)

The same 39 decisions at a smaller beam (`search-recall run corpus/search --beam N`). The default
beam prunes almost nothing at depth 1, so these runs are where the ranking is exercised.

| beam | generated | hit | recall | inline-function | misplace-item | obscure-name | depth 1 | depth 2 | dropped at |
|---|---|---|---|---|---|---|---|---|---|
| 20 | 31 | 30 | 0.97 | 4/5 | 13/13 | 14/15 | 24/24 | 6/7 | dominated 1 |
| 5 | 31 | 29 | 0.94 | 3/5 | 12/13 | 14/15 | 24/24 | 5/7 | dominated 2 |
| 3 | 31 | 27 | 0.87 | 2/5 | 12/13 | 13/15 | 22/24 | 5/7 | dominated 4 |
| 1 | 31 | 23 | 0.74 | 0/5 | 12/13 | 12/15 | 19/24 | 4/7 | dominated 8 |

(`inline-variable` is 5/5 at beams 20, 5 and 3, and 3/5 at beam 1.) Every miss is `dominated`,
never `rank`: the dominance rule, not the score order, is what removes the extract-function
restore. Recall at beam 3 is 0.87 and at beam 1 it is 0.74 (a `hold`), so H6 does not rest on
the beam being wide: it holds down to 3, and the weak spot is one stage.

## Exhaustive (secondary truth)

`search-recall run corpus/search --exhaustive` (beam 200, no wall limit, 18.4 s, 879 checks): every generated
best survives (31 of 31, recall@200 1.00), so nothing is lost to the HIR check and the miss above
is the pruning's. The final ranking by full reward puts the best at:

| kind (generated decisions) | final rank 0 | final rank 1 to 3 | later |
|---|---|---|---|
| obscure-name (15) | 12 | 2 (ranks 1 and 2) | 1 (rank 9, with an inline-function) |
| misplace-item (13) | 12 | 1 (rank 3, with an inline-function) | 0 |
| inline-variable (5) | 3 | 2 (rank 2 and 2) | 0 |
| inline-function (5) | 0 | 4 (ranks 1, 2, 3 and 3) | 1 (rank 9, with an obscure-name) |

The restore of an inline-function plant is never the top reward: the score weights are untuned
(KNOWN LIMITATIONS) and a bigger edit loses to a smaller one. top-3 is 0.72 overall and 0.33 for
inline-function (the count over measured decisions, so a best that was not generated counts
against it).

## Noul audit

`MetricJudge` finds no flaw, so the audit is empty: 0 flawed children were checked anyway and 0
of them passed. The numbers are meaningful only with a `ReplayJudge` fixture or a real judge; the
CLI takes `judge: {kind: "replay", file}` and `search_recall` counts both.

## Outer check

`search-recall run corpus/search --outer-check` runs `cargo check --workspace --all-targets` on a
copy of the workspace for each `Decided` candidate (at most 3 per decision) and counts those that
report an error the unedited workspace does not:

| corpus | decided candidates | failed | rate |
|---|---|---|---|
| small bases | 114 | 14 | 0.12 |
| ryo-rs | 30 | 2 | 0.07 |

All 14 failures are E0255 (a name defined by an item and by a `use` in the same module), in 11
decisions: the 9 single `misplace-item` decisions and 2 pairs that contain one. They are the 2nd
or 3rd candidate of the decision, never the best: 0 of the 39 best candidates failed. The
failing candidate moves the item back into the file that already re-exports it and leaves the
`use`. rust-analyzer's diagnostics pass it (the search's check misses E0255) and only the outer
`cargo check` stops it, which is review finding 2 (the outer check) shown with a number. A
decision that plants no `misplace-item` had no failing candidate. (The pairs `inline-function`
plus `misplace-item` and `inline-variable` plus `misplace-item` had none either: their `Decided`
candidates did not include the bad move.)

On ryo-rs (a verification repository used as a large sample; run 3 of section "ryo-rs (large
run)") 2 of 30 decided candidates failed, in 10 decisions (`inline-variable-0` had no candidate),
both in `misplace-item` decisions: candidate 0 of `misplace-item-0` reports E0255 at
`crates/ryo-agent/src/decide.rs:147`, candidate 1 of `misplace-item-2` reports E0204 (a `Copy`
impl on a type with a field that is not `Copy`) at `crates/ryo-agent/src/decide.rs:150` (paths
inside ryo-rs). The harness counts 1 of the 10 first candidates of a decision as failed: candidate
0 of `misplace-item-0`, a decision whose planted best was not generated. rust-analyzer's
diagnostics passed both candidates and only the outer `cargo check` rejected them: two more errors
the search's check misses (CHECK_LEDGER Open work M-10).

## ryo-rs (large run)

ryo-rs is a verification repository used as a large sample. The 11 cases of step 1 of section
"Large runs" (ryo-rs at its commit 67b0ebfb, `probes = []`) were run on 2026-11-01 with the
options of steps 2 and 3, on 16 cores and 30 GiB with the machine otherwise idle, three runs one
after the other (no two at once): run 1 at beam 20, run 2 with `--exhaustive --limit 10`, run 3 as
run 1 with `--outer-check`. Run 3 gives the same table and the same beam rank, fate and final rank
for every decision as run 1; its outer check is in section "Outer check". The numbers of this
section are from run 1 unless a run is named.

Run 1 (360,538 ms, 172 checks):

| set | decisions | skipped | generated | hit | recall@20 | hit/all | generation | top-3 |
|---|---|---|---|---|---|---|---|---|
| all | 11 | 0 | 6 | 6 | 1.00 | 0.55 | 0.55 | 0.27 |
| kind inline-variable | 4 | 0 | 1 | 1 | 1.00 | 0.25 | 0.25 | 0.25 |
| kind misplace-item | 3 | 0 | 1 | 1 | 1.00 | 0.33 | 0.33 | 0.33 |
| kind obscure-name | 6 | 0 | 5 | 5 | 1.00 | 0.83 | 0.83 | 0.17 |
| depth 1 | 9 | 0 | 5 | 5 | 1.00 | 0.56 | 0.56 | 0.33 |
| depth 2 | 2 | 0 | 1 | 1 | 1.00 | 0.50 | 0.50 | 0.00 |

(The one workspace row, `ws ryo-rs`, equals `all`.)

Verdict: **insufficient**: recall@20 1.00 over 6 generated decisions, fewer than the 30 compared
decisions that decide anything. 0 misses in 6 put the true miss rate at most 0.39 at 95 % (exact
binomial, one-sided), so the run adds no evidence for or against H6 beyond the small bases.

| decision | kinds | depth | generated | hit | beam rank | fate | final rank | proposed | kept | wall ms |
|---|---|---|---|---|---|---|---|---|---|---|
| ryo-rs/inline-variable-0 | inline-variable | 1 | no | no | - | no-candidates | - |  |  | 1504 |
| ryo-rs/inline-variable-1 | inline-variable | 1 | yes | yes | 0 | passed | 0 | 6 | 6 | 6845 |
| ryo-rs/inline-variable-2 | inline-variable | 1 | no | no | - | - | - | 15 | 15 | 4668 |
| ryo-rs/misplace-item-0 | misplace-item | 1 | no | no | - | - | - | 5 | 5 | 4741 |
| ryo-rs/misplace-item-1 | misplace-item | 1 | no | no | - | - | - | 3 | 3 | 3642 |
| ryo-rs/misplace-item-2 | misplace-item | 1 | yes | yes | 0 | passed | 0 | 7 | 7 | 8986 |
| ryo-rs/obscure-name-0 | obscure-name | 1 | yes | yes | 3 | passed | 3 | 17 | 17 | 13086 |
| ryo-rs/obscure-name-1 | obscure-name | 1 | yes | yes | 2 | passed | 5 | 16 | 16 | 44457 |
| ryo-rs/obscure-name-2 | obscure-name | 1 | yes | yes | 1 | passed | 1 | 12 | 12 | 32150 |
| ryo-rs/pair-obscure-name-inline-variable-0 | obscure-name+inline-variable | 2 | no | no | - | - | - | 44/175 | 20/20 | 79077 |
| ryo-rs/pair-obscure-name-obscure-name-0 | obscure-name+obscure-name | 2 | yes | yes | 13 | passed | 12 | 27/107 | 20/20 | 90682 |

Every generated best survived the pruning (dropped at: none), and the noul audit is empty (0
flawed children checked anyway). At depth 1 the beam does not fill (at most 17 children proposed); in
both pairs it is full at both depths, and the obscure-name pair keeps its best at beam rank 13.

Generation is 6 of 11 (0.55; the small bases give 0.79). The five decisions whose best is never
generated are `inline-variable-0` (no candidate at all, fate `no-candidates`), `inline-variable-2`,
`misplace-item-0`, `misplace-item-1` and the pair `obscure-name` plus `inline-variable`. Their
cause is not investigated (CHECK_LEDGER Open work S-4); whether the inline-variable ones are the
`let`-initializer gap of section "Generation" is not checked.

Run 2 (`--exhaustive --limit 10`: beam 200, no wall limit, the first 10 cases, which leaves out
`pair-obscure-name-obscure-name-0`; 466,780 ms, 304 checks): 5 of the 10 decisions generate the
best and all 5 survive (recall@200 1.00, generation 0.50, top-3 0.30, verdict insufficient). The
9 depth-1 decisions have the same generated, beam rank and final rank as in run 1. The pair
`obscure-name` plus `inline-variable` keeps every child at depth 1 (44 of 44, and 169 of 175 at
depth 2) and still does not generate its best, so that miss is the expander's, not the beam's.

Each of the three runs hit one panic inside rust-analyzer's syntax factory, raised from the
expander, and went on past it; the harness does not count it, so the tables above do not show it
(CHECK_LEDGER Open work S-3 and R-5).

## Perf triggers (decision D4)

The table is filled by every run; the small bases cannot fire trigger 1 to 3 (they are tiny). The
ryo-rs column is run 1 of section "ryo-rs (large run)" (ryo-rs is a verification repository used
as a large sample; 16 cores, 30 GiB, the machine otherwise idle).

| trigger | limit | small bases | ryo-rs |
|---|---|---|---|
| 1 median decision | 300,000 ms | 138 ms (not fired) | 8,986 ms (not fired) |
| 2 median warm check (Affected) | 5,000 ms | 5 ms over 565 checks (not fired) | 1,476 ms over 172 checks (not fired) |
| 3 RSS after 1,000 checks, or check time x1.5 | 8 GiB, or 1,500 permille of the first 100 checks | 595,024 kB after 565 checks; both halves not measured: fewer than 1,000 checks | 5,653,460 kB after 172 checks; both halves not measured: fewer than 1,000 checks |
| 4 one recall run | 600,000 ms | 14,556 ms (not fired) | 360,538 ms (not fired) |

The other two ryo-rs runs: run 2 (`--exhaustive --limit 10`) 9,810 ms, 847 ms over 304 checks,
5,701,156 kB after 304 checks (both halves of trigger 3 not measured) and 466,780 ms, none fired;
run 3 (`--outer-check`) 10,149 ms, 1,265 ms over 172 checks, 5,645,592 kB after 172 checks (not
measured) and 983,033 ms, trigger 4 printed as fired. Triggers 1 and 2 fired in no run, and
trigger 3 is not measured on ryo-rs: no run reached 1,000 checks.

Trigger 4 in run 3: the decisions of run 3 take about as long as those of run 1 (their wall times
sum to 277,368 ms against 289,838 ms), and the rest of the run is 705,665 ms against 70,700 ms
in run 1. The difference, 634,965 ms, is the outer `cargo check` of the 30 decided candidates
(about 21 s per candidate, derived from these sums, not measured per candidate), which runs after
each search and outside it. D4
states trigger 4 for one recall run in CI; none of the three runs ran in CI, and the recall run
without the outer check (run 1) is below the limit. By D4's text, trigger 4 therefore opens no perf
task, and none of the steps (a) to (d) of its work order would shorten the outer check. Whether a
run with `--outer-check` counts for trigger 4 is not stated by D4, and the harness prints it as
fired whatever the run (CHECK_LEDGER Open work S-5).

## Large runs (not run in the default test suite)

The ryo-rs corpus is not run in the
default test suite; it needs a large machine, with `<BASE>` an exported ryo-rs
tree and `<OUT>` a directory without a `target` path component:

1. Plants: `cargo run -q -p ryo-v2-oracle --bin search-recall -- gen ryo-rs --base <BASE> --out
   <OUT>/ryo-rs --seeds 3 --pairs 2` (at least 10 decisions). Done 2026-10-31 on ryo-rs (a
   verification repository used as a large sample) at its commit 67b0ebfb, in 61 s: 11 decisions,
   9 at depth 1 (3 obscure-name, 3 inline-variable, 3 misplace-item) and 2 pairs (obscure-name plus
   obscure-name, obscure-name plus inline-variable). Sites found: obscure-name 172, misplace-item
   97, inline-variable 10, inline-function 0 (its 3 seeds are skipped). The cases are not
   committed (they name `<BASE>`) and have `probes = []`.
2. The run (longer than 10 minutes): `cargo run -q -p ryo-v2-oracle --bin
   search-recall -- run <OUT>/ryo-rs --report <OUT>/ryo-rs.json --markdown <OUT>/ryo-rs.md`.
   Done 2026-11-01 on ryo-rs (a verification repository used as a large sample) with the 11
   cases of step 1, in 360,538 ms, on 16 cores and 30 GiB with the machine otherwise idle.
3. The same with `--exhaustive --limit 10`, and with `--outer-check`, then paste the tables here.
   Done 2026-11-01 on the same machine, after step 2 and one after the other (no two runs at
   once): `--exhaustive --limit 10` in 466,780 ms, `--outer-check` in 983,033 ms. The tables are
   in sections "ryo-rs (large run)", "Outer check" and "Perf triggers".

## KNOWN LIMITATIONS

- **Plant sites are chosen so the expander can name the restore.** An obscure-name or
  inline-variable site is one where the name rust-analyzer's `NameGenerator` would suggest (the
  callee or method name, or the parameter name of a uniquely defined callee) is the name the clean
  code uses. This biases generation upward; recall@20 measures the pruning, and the generation
  rate is reported beside it and must not be folded in.
- **Whitespace and alpha-rename matching.** A state matches the clean text up to whitespace runs
  and the name of an extracted function. A reordered parameter list or a different type is a miss
  (counted as not generated). An inline-function plant therefore needs the helper in the shape the
  expander writes: `let x = e; x`, parameters in the order the caller defines them, by value for
  what is moved and `&name` for what is borrowed.
- **Syntactic extensibility.** The static metric counts edit sites by name, not by type. The
  small-base cases carry hand-written probes (section "Probes"), but every edit the M4 expander
  proposes keeps the number of probe sites, so the extensibility term is still 0 for every
  candidate and the ranking is driven by the `clean` metric and the edit size. The ryo-rs cases
  are generated with `probes = []`.
- **Add-derive is not planted.** Its best has no definition independent of the metrics.
- **Score weights are untuned** (`DEFAULT_WEIGHTS`): the restore of an inline-function plant is
  never the top reward (above). They are to be set from this table, not before.
- **A small corpus.** 39 decisions on tiny code, 28 of them depth 1 where the beam does not fill;
  the narrow-beam runs and the depth-2 pairs carry the evidence. A larger base is the large run,
  whose 11 decisions on ryo-rs (6 compared) decide nothing either.
- **The `let`-initializer gap of the expander** limits which inline-variable plants are generated
  (above).
