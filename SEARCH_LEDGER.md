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
it and the guard that fails when the value is broken. All were accepted 2026-10-31; D4's trigger 4
was restated 2026-11-01 after the first ryo-rs runs (its "revisit when"). A row is
changed, with a new measurement, when its "revisit when" is met; a rejected option is not taken up
again without a new measurement. The measurements are from `docs/bench-m2.md` (assert off,
Affected scope, warm session; 12 threads, 30 GiB). ryo-rs is a verification repository used as a
large sample.

| id | decision (current value) | why it holds today (measurement, date) | guard (constant / test / contract clause) | revisit when | rejected options |
|---|---|---|---|---|---|
| D1 time budget of one decision | One decision is one `search_decision` call (expand, prune, verify the survivors with `Verifier::check`, repeat to a depth, rank). Wall time 300 s soft and 360 s hard; 360 s = 20 x 3 x 6 s, the product of `beam`, `max_depth` and `per_candidate_ms` of `DEFAULT_SEARCH_BUDGET`. Soft: no new depth starts once 300 s have passed (`SearchReport.soft_stopped`); hard: the search stops at once (`SearchReport.budget_exhausted`). One check is cut off after 6 s (`per_candidate_ms`, `CheckError::TimedOut`); the candidate is recorded as `Blocking::BudgetExhausted { limit_ms }` and the search goes on. Expand plus static metrics: 10 s per depth (200 candidates x 50 ms). Judge: one call per depth, 30 s; the metric judge takes 0. At the hard limit the result is `Decided` with what has been verified so far; with nothing passed it is `Failed` with `budget_exhausted: true`. The expand and judge allowances are recorded, not enforced, because the metric judge and the syntactic assessor are far below them. Wall time is read from the `Clock` port, so tests drive the budget with a fake clock. | 2026-10-31, `docs/bench-m2.md`: one check takes 0.3 to 4.3 s, one revert 0.04 to 0.36 s, check plus revert 2 to 3 s at the median. At most 20 candidates per depth over at most 3 depths is at most 60 checks, which is 120 to 180 s at the median, so 300 s leaves the second half of the budget for expansion, the judge and the slow tail, and 360 s is derived from the knobs, not chosen separately. 6 s is a little above the measured 4.3 s maximum; a check beyond it is a candidate Vision principle 15 (5 s or less per candidate) already rejects. A caller who waits 6 minutes gets the verified candidates, not an error. Consequence: a base where most checks exceed 6 s yields `Failed`, the signal to start the perf work of D4. | `DEFAULT_SEARCH_BUDGET` (beam 20, `max_expand` 200, `beam_parents` 4, `max_depth` 3, `per_candidate_ms` 6,000, `soft_ms` 300,000, `hard_ms` 360,000), `EXPAND_DEPTH_MS` = 10,000, `JUDGE_DEPTH_MS` = 30,000; contract clause C1 of `DEFAULT_SEARCH_BUDGET`, test `app::search::tests::default_search_budget_is_the_decided_numbers` (pins every value, including the allowances); behaviour: `soft_budget_stops_after_the_depth`, `budget_exhausted_returns_verified_so_far`, `hard_budget_with_nothing_verified_is_failed_exhausted`, `check_timeout_is_budget_exhausted_blocking` (core), `tests/timeout.rs check_past_timeout_is_timed_out_and_session_usable` (ra). The allowances have no behaviour guard: they are not enforced. | The median decision on the ryo-rs corpus exceeds 300 s (D4, trigger 1); the measured slowest check exceeds 6 s on a base the user cares about; a real judge (an LLM) is added, whose 30 s per depth becomes an enforced limit. | No budget (the M3 state): one decision's duration cannot be stated, so principle 15 cannot be checked at all. A candidate-count budget only (beam times depth): on a large base one check takes more than 4 s as a matter of course, so the same count means 2 minutes or 10 and the time cannot be read from it. |
| D2 revert assertion in production | `Verifier::check` applies edits to a session, diagnoses and reverts. The text fingerprint comparison of the touched files runs on every check (cheap, O(touched bytes)). The diagnostic re-assert (diagnose the touched files again and compare with the baseline) runs on every 50th check (`RaConfig.assert_revert_every = Some(50)`) and once at the end of every decision, before the Resolution is returned (`RevertAudit::audit_revert`). The bench and the oracle keep `Some(1)`. A mismatch poisons the session (`CheckError::RevertMismatch`, then `Poisoned`); the caller reloads it, which costs about 40 s (warm load 3.2 s plus baseline 35 s), outside the budget of D1 and recorded as evidence. A mismatch found at the audit rejects the whole decision (`SearchError::RevertAudit`), not only the last check, because it cannot say when the drift began. `RevertAudit` is a port beside `Verifier`; the fake verifier of the core tests implements it. | 2026-10-31, `docs/bench-m2.md`: the re-assert costs 1.4 to 2.1 s, which doubles the cost of a check, so every check cannot pay it in a search of 60. Every 50 checks it is 30 to 40 ms per check, 1 to 2 % of a check. The end-of-decision audit costs one re-assert (about 2 s of 360 s) and means a `Decided` result has always been followed by a clean diagnostic comparison of the session it came from. The fingerprint catches text drift (review finding V5) on every check; what it cannot see is the analyzer's derived state drifting while the text is right, and sampling plus the final audit bounds how long such a drift can go unnoticed. | `ASSERT_REVERT_EVERY` = 50, `RaConfig.assert_revert_every`; contract clauses C1 to C3 of `RaAnalyzer::check` and C1, C2 of `RevertAudit for RaAnalyzer`; tests `tests/verifier.rs check_leaves_text_and_baseline_at_base`, `tests/verifier.rs assert_revert_every_samples_the_diagnostic_assert`, `tests/timeout.rs audit_revert_is_ok_after_checks_and_refuses_poisoned`, core `revert_audit_runs_once_at_end`; the value 50 is pinned by `default_search_budget_is_the_decided_numbers`. The sampling period is a constant; changing it needs a new bench of the poison rate. | A poison (revert mismatch) is seen in production use: lower the period or audit every depth; the re-assert gets cheaper (item-level scope, D4 order step 1), which allows a shorter period; the check time grows by 1.5 times over a long run (D4, trigger 3). | Re-assert on every check: the cost of a check doubles (the 3 to 6 s of the bench) and the 6 s cut-off of D1 would fire on healthy checks. Fingerprint only: if salsa state drifts without the text changing nothing detects it and `Decided` is silently wrong, which is the failure the engine exists to avoid. |
| D3 parallelism and memory | One decision uses one session (one loaded rust-analyzer database) and verifies its candidates one after another, siblings consecutively. The only parallelism is inside a check: the diagnostic threads (`RaConfig.threads`, default the available parallelism) split the files of the scope. More sessions are started only after a D4 trigger fires, and then at most `N = max(1, floor((RAM_GB - 8) / 6))` (`RESERVED_RAM_GB = 8`, `SESSION_RAM_GB = 6`; N = 3 on 30 GB), stated by `max_sessions(ram_gb)` in `ryo-v2-core`. The `Verifier` port keeps `&mut Session`: one check at a time per session, enforced by the type. A caller that wants two decisions at once starts two processes and owns the memory budget. `max_sessions` exists as a function and a test, not as a feature. | 2026-10-31, `docs/bench-m2.md` (30 GiB, 12 threads): one session of the base of ryo-rs takes about 5 GB and 60 repeated runs grew it by about 0.75 GB, so 6 GB per session; a second `AnalysisHost` doubles the resident set size; salsa gives no writable fork, so edits are applied to the one database and reverted. The diagnostics of one check already run on all cores by file, so a second session adds parallelism only for the cores one check leaves idle (an Amdahl bound, not measured). The memory cap decides first: 8 GB stay free for the rest of the machine, so 3 sessions on 30 GB and a best case of 3 times. Serial checks keep the search deterministic and the revert audit simple (one session, one history); races and poisoning with several sessions are a class of failure to take on only when the serial path is correct and too slow. | `max_sessions`, `SESSION_RAM_GB` = 6, `RESERVED_RAM_GB` = 8; contract clause C1 of `max_sessions`, test `app::search::tests::max_sessions_follows_decision_d3` (30 gives 3, 0 and 8 and 14 give 1, 20 gives 2, 64 gives 9); the two constants are pinned by `default_search_budget_is_the_decided_numbers`; the `&mut Session` of the `Verifier` port is enforced by the type. | A D4 trigger fires (the median decision exceeds 300 s, or the median warm check exceeds 5 s) after the cheaper steps of its order are done; the backend offers a cheap, writable fork; the machine has less than 14 GB (the formula gives 1; one session needs 6 GB to be safe). | N sessions now: the memory caps the gain at 3 times on a 30 GB machine, and the race and poison handling would be built before the serial path is shown correct and too slow. Several forks in one session: the backend (salsa) has no writable fork, so this is not buildable without replacing the backend. |
| D4 when to start perf work | No perf work during M4; the harness only measures. A perf task is opened when any of these happens on the ryo-rs decision corpus (the Perf triggers section below shows their status): (1) the median wall time of a decision exceeds 300 s (the soft limit of D1); (2) the median warm check in Affected scope exceeds 5 s (Vision principle 15); (3) after 1,000 checks the resident set size exceeds 8 GB, or the check time has grown by 1.5 times (review finding V4); (4) one recall run of the planted corpus takes more than 10 minutes without its outer check: the time judged is the wall time of one `search-recall run` less the time of the oracle's outer `cargo check` (the baseline of each workspace and every decided candidate under `--outer-check`), session loads included; the run judged is any run whose search budget is nowhere larger than `DEFAULT_SEARCH_BUDGET` (a narrower beam is judged, `--exhaustive` or a wider beam is not measured), on any corpus and any machine, the ledger naming the machine beside the number. Work order when a trigger fires, cheapest and most effective first: (a) item-level scope: diagnose only the files that reference the changed item (434 of 504 files today; tens expected, an estimate), with the missed-error risk measured by the oracle; (b) order siblings so that checks that touch the same files run together; (c) parent-to-child deltas inside the adapter (skip reverting to the base between a parent and its child); (d) N sessions (D3). Each step follows the usual profiling procedure: save a baseline, take a flame graph, change the hot path only, bench again. The constants live in `ryo-v2-core::app::search` so the harness and this row cannot disagree. The ryo-rs run is outside the default test suite, so trigger 4 is judged on the small bases and on each recorded large run alike. | 2026-10-31: the numbers behind D1 to D3 come from small and medium bases and the decision corpus of ryo-rs does not exist yet at scale; perf work started without a number to move is guesswork, and perf work never started leaves the budget of D1 unwatched. Item-level scope attacks the largest factor (files diagnosed per check); deltas only remove the revert, which is 0.04 to 0.36 s of a 0.3 to 4.3 s check, about 10 %. The triggers are the limits D1 and principle 15 state, so each says "a promise we made is broken" and not "something could be faster". 2026-11-01, trigger 4 restated (section "Perf triggers"; ryo-rs, a verification repository used as a large sample, 16 cores, 30 GiB): run 1 (beam 20) took 360,538 ms; run 3, the same with `--outer-check`, took 983,033 ms, of which about 635 s (634,965 ms, derived from the sums of the decisions' wall times, about 21 s per decided candidate) is the outer check, which runs after each search and outside it. Its decisions took 277,368 ms against 289,838 ms in run 1, so the search was no slower; read as the whole run, trigger 4 would open a perf task that none of the steps (a) to (d) can serve, which is "something is slow", not "a promise is broken". The outer check is the oracle's second opinion of the verifier, not a cost the user of a decision pays. An exhaustive run lifts the wall limits of D1 on purpose, so its time breaks no promise; a narrower beam can only be faster, so a fire there is a fire at the default too. | Trigger 1: `TRIGGER_DECISION_MEDIAN_MS` = 300,000. Trigger 2: `TRIGGER_CHECK_MEDIAN_MS` = 5,000. Trigger 3, memory: `TRIGGER_RSS_KB` = 8 x 1024 x 1024 and `TRIGGER_AFTER_CHECKS` = 1,000; trigger 3, growth: `TRIGGER_CHECK_GROWTH_PERMILLE` = 1,500 against the median of the last `search_recall::GROWTH_WINDOW` = 100 checks in thousandths of the median of the first 100, read after `TRIGGER_AFTER_CHECKS` checks by `search_recall::check_growth_permille` through `trigger_lines`, test `trigger_3_reads_the_growth_limit` (the limit itself does not fire, one ms above it does, one check fewer is not measured) and `check_growth_compares_the_ends_of_the_run`; the checks of all measured decisions are flattened in row order, so on a corpus of several workspaces (one session each) the two windows can come from different sessions: the growth half is meant for the one-workspace ryo-rs run. Trigger 4: `TRIGGER_HARNESS_MS` = 600,000 against `RunInfo::search_ms` (the run less `RunInfo.outer_check_ms`, which `search-recall run` times around every outer `cargo check`), judged when `search_recall::trigger_4_judges` accepts the run's budget; contract clause C2 of `trigger_lines`, tests `trigger_4_reads_the_search_time_of_a_default_run` (the limit itself does not fire, one ms above it does, run 3's 983,033 ms with 634,965 ms of outer check does not, a narrower beam is judged, an exhaustive or unknown budget is not measured; the JSON report carries `search_ms` and `outer_check_ms`) and `trigger_4_judges_runs_within_the_default_budget` (each knob one above the default refuses, `max_depth` and `audit_every` are not compared); the run's budget and outer-check time reach `RunInfo` through `RunInfo::of_run`, test `run_info_of_a_run_carries_budget_and_outer_time`. Values pinned by `default_search_budget_is_the_decided_numbers`; `search_recall::trigger_lines` prints triggers 1, 2, 3 (memory and growth) and 4, pinned by test `triggers_follow_the_decided_numbers`. | The first full ryo-rs run is in this ledger: compare each trigger with its number; a trigger fires (this row is then superseded by the perf task's own record); a base larger than ryo-rs is the target. | Optimise now (parent-to-child deltas, N sessions): deltas save about 10 % of a check and add a second state to verify (the child's state on top of the parent's), a new way to be wrong. Ignore perf and go on to M5: nobody would notice when the budget of D1 stops being met. Trigger 4 on the whole wall time, outer check included (the harness until 2026-11-01): run 3 fires it through the oracle's own `cargo check` while the search is as fast as in run 1, and no step of the work order shortens that check. Leave runs with `--outer-check` out of trigger 4: such a run still measures its search, and that measurement would be lost. Judge trigger 4 only in CI: no CI run covers the large corpus, so the trigger would never be judged where it can fire. |

## Counts

Corpus and runs of the small bases: 2026-11-01, branch `fix/expander-gen` over main 63db161,
rustc 1.98.1, ra_ap 0.0.358, 16 cores and 30 GiB with other runs possibly on the machine,
proc-macro on, `RaConfig.assert_revert_every = 50` (decision D2), `MetricJudge`,
`SyntaxAssessor`, Affected scope, one session per workspace, with probes. "before" is the expander
of main 63db161 on the same 42 cases; "after" is the expander of this change (section
"Generation"). The rows of 2026-10-31 (39 cases, before the `gen-lab` cases and the detectors) are
in the git history of this file. The two rows marked "second round" are of 2026-11-01, branch
`fix/ryo-rs-causes` over main fff60ef, on the 45 cases with the three `shape-lab` cases, the same
settings and machine: the expander of main fff60ef, and the expander after the causes of the
ryo-rs run (section "Generation, second round").

| corpus | expander | decisions | depth 2 | skipped | generated | hit | recall@20 | generation | status |
|---|---|---|---|---|---|---|---|---|---|
| small bases (below) | before | 42 | 10 | 0 | 31 | 30 | 0.97 | 0.74 | verdict adopt, 31 compared |
| small bases (below) | after | 42 | 10 | 0 | 42 | 38 | 0.90 | 1.00 | verdict adopt, 42 compared |
| small bases (below) | second round, before (main fff60ef) | 45 | 10 | 0 | 42 | 38 | 0.90 | 0.93 | verdict adopt, 42 compared |
| small bases (below) | second round, after | 45 | 10 | 0 | 45 | 41 | 0.91 | 1.00 | verdict adopt, 45 compared |
| ryo-rs | before | 11 | 2 | 0 | 6 | 6 | 1.00 | 0.55 | verdict insufficient, 6 compared |
| ryo-rs | after (main fff60ef) | 11 | 2 | 0 | 7 | 6 | 0.86 | 0.64 | verdict insufficient, 7 compared |
| ryo-rs | second round, after | pending | | | | | | | |

ryo-rs here is ryo-rs (a verification repository used as a large sample); its run, with the
machine it ran on, is in section "ryo-rs (large run)".

The small bases are tiny (the four `corpus/base` workspaces hold 691 lines in all) and give almost
no site; most decisions come from `search-lab`, a hand-written workspace of two crates (`geo`: 4
files, `app`: 2 files, 453 lines, rustfmt-formatted, compiles clean with and without the
planted edits) written to hold sites of every kind.

| workspace | decisions | notes |
|---|---|---|
| `search/base/search-lab` | 38 | 28 at depth 1 (7 obscure-name, 9 inline-variable, 3 inline-function, 9 misplace-item) and 10 pairs |
| `search/base/gen-lab` | 3 | hand cases, depth 1: two misplace-item (a re-export under a comment, an item with a trailing comment) and one inline-variable into a struct-literal field value, the shapes of section "Generation" |
| `search/base/shape-lab` | 3 | hand cases, depth 1: one misplace-item of an item with a `///` doc line, one inline-variable of a reference to a nested field and one of a literal, each folded into a call argument; the shapes of section "Generation, second round" |
| `base/breakage-lab` | 1 | one obscure-name site (`store/src/lib.rs`); no other kind has a site |
| `base/cfg-test-lab`, `base/macro-lab`, `base/extract-trait-multicrate` | 0 | no site of any kind |

Run at beam 20 (`search-recall run corpus/search`, 25.2 s after, 24.6 s before):

| set | decisions | skipped | before: generated / hit / recall@20 / top-3 | after: generated | hit | recall@20 | hit/all | generation | top-3 |
|---|---|---|---|---|---|---|---|---|---|
| all | 42 | 0 | 31 / 30 / 0.97 / 0.67 | 42 | 38 | 0.90 | 0.90 | 1.00 | 0.86 |
| ws breakage-lab | 1 | 0 | 1 / 1 / 1.00 / 1.00 | 1 | 1 | 1.00 | 1.00 | 1.00 | 1.00 |
| ws gen-lab | 3 | 0 | 0 / 0 / - / 0.00 | 3 | 3 | 1.00 | 1.00 | 1.00 | 1.00 |
| ws search-lab | 38 | 0 | 30 / 29 / 0.97 / 0.71 | 38 | 34 | 0.89 | 0.89 | 1.00 | 0.84 |
| kind inline-function | 6 | 0 | 5 / 4 / 0.80 / 0.33 | 6 | 4 | 0.67 | 0.67 | 1.00 | 0.33 |
| kind inline-variable | 15 | 0 | 5 / 5 / 1.00 / 0.33 | 15 | 11 | 0.73 | 0.73 | 1.00 | 0.73 |
| kind misplace-item | 16 | 0 | 13 / 13 / 1.00 / 0.75 | 16 | 15 | 0.94 | 0.94 | 1.00 | 0.88 |
| kind obscure-name | 15 | 0 | 15 / 14 / 0.93 / 0.93 | 15 | 14 | 0.93 | 0.93 | 1.00 | 0.93 |
| depth 1 | 32 | 0 | 24 / 24 / 1.00 / 0.72 | 32 | 32 | 1.00 | 1.00 | 1.00 | 0.97 |
| depth 2 | 10 | 0 | 7 / 6 / 0.86 / 0.50 | 10 | 6 | 0.60 | 0.60 | 1.00 | 0.50 |

A kind row counts the decisions that plant the kind, so a pair is in two rows.

Verdict: **adopt** on the numbers after the change (recall@20 0.90 over 42 generated decisions,
above the 0.8 of adoption), with three reservations that the next sections spell out. 4 misses in
42 put the true miss rate at most 0.21 at 95 % (exact binomial, one-sided); at depth 1, 0 misses
in 32 put it at most 0.09. Before the change it was 0.97 over 31 (1 miss, at most 0.14). Recall
fell because the 11 bests the expander now generates include three depth-2 bests the pruning
drops (section "Dropped at"), not because a best generated before is lost: the 31 decisions
generated before keep their generated, hit and fate.

1. At depth 1 the beam never fills: the expander proposes 1 to 11 children, so recall@20 at
   depth 1 cannot be below 1.00. The pruning is exercised at depth 2, where 16 to 80 children are
   proposed and the beam is full in 9 of the 10 pairs, and by the narrow-beam runs below. At depth
   2 recall@20 is 0.60 (6 of 10).
2. `search-lab` is small and written for this harness: its sites are natural to the generator.
3. The 3 `gen-lab` cases are written by hand for the causes of section "Generation"; they show
   those causes, they are not a sample.

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
The 3 hand cases of `gen-lab` (2026-11-01) carry one `add-field` probe each, on gen-lab's own
`Slot`; that test does not load `gen-lab` yet.

| case kind (decisions) | probes | add-variant | add-field | per case |
|---|---|---|---|---|
| obscure-name, depth 1 (8, one in breakage-lab) | 10 | 5 | 5 | 1 to 2 |
| inline-variable, depth 1 (9) | 12 | 4 | 8 | 1 to 2 |
| inline-function, depth 1 (3) | 4 | 1 | 3 | 1 to 2 |
| misplace-item, depth 1 (9) | 11 | 2 | 9 | 1 to 2 |
| pairs, depth 2 (10) | 24 | 6 | 18 | 2 to 3 |
| hand cases of `gen-lab`, depth 1 (3) | 3 | 0 | 3 | 1 |
| all (42) | 64 | 18 | 46 | |

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

After the change (section "Generation"). The table of the expander before it differs in the 11
rows that are now generated (section "Generation" lists them) and in the proposed counts below.

| decision | kinds | depth | generated | hit | beam rank | fate | final rank | proposed | kept | wall ms |
|---|---|---|---|---|---|---|---|---|---|---|
| breakage-lab/obscure-name-0 | obscure-name | 1 | yes | yes | 3 | passed | 1 | 6 | 6 | 69 |
| gen-lab/inline-variable-field | inline-variable | 1 | yes | yes | 1 | passed | 1 | 3 | 3 | 15 |
| gen-lab/misplace-item-comment-above | misplace-item | 1 | yes | yes | 0 | passed | 0 | 2 | 2 | 10 |
| gen-lab/misplace-item-comment-after | misplace-item | 1 | yes | yes | 0 | passed | 0 | 1 | 1 | 4 |
| search-lab/inline-function-0 | inline-function | 1 | yes | yes | 3 | passed | 3 | 10 | 10 | 101 |
| search-lab/inline-function-1 | inline-function | 1 | yes | yes | 2 | passed | 2 | 7 | 7 | 390 |
| search-lab/inline-function-2 | inline-function | 1 | yes | yes | 1 | passed | 1 | 9 | 9 | 264 |
| search-lab/inline-variable-0 | inline-variable | 1 | yes | yes | 2 | passed | 2 | 8 | 8 | 69 |
| search-lab/inline-variable-1 | inline-variable | 1 | yes | yes | 2 | passed | 2 | 8 | 8 | 208 |
| search-lab/inline-variable-2 | inline-variable | 1 | yes | yes | 0 | passed | 0 | 6 | 6 | 173 |
| search-lab/inline-variable-3 | inline-variable | 1 | yes | yes | 0 | passed | 0 | 4 | 4 | 35 |
| search-lab/inline-variable-4 | inline-variable | 1 | yes | yes | 0 | passed | 0 | 8 | 8 | 174 |
| search-lab/inline-variable-5 | inline-variable | 1 | yes | yes | 0 | passed | 0 | 7 | 7 | 171 |
| search-lab/inline-variable-6 | inline-variable | 1 | yes | yes | 0 | passed | 0 | 6 | 6 | 140 |
| search-lab/inline-variable-7 | inline-variable | 1 | yes | yes | 0 | passed | 0 | 7 | 7 | 165 |
| search-lab/inline-variable-8 | inline-variable | 1 | yes | yes | 1 | passed | 1 | 7 | 7 | 167 |
| search-lab/misplace-item-0 | misplace-item | 1 | yes | yes | 0 | passed | 0 | 2 | 2 | 34 |
| search-lab/misplace-item-1 | misplace-item | 1 | yes | yes | 0 | passed | 0 | 4 | 4 | 153 |
| search-lab/misplace-item-2 | misplace-item | 1 | yes | yes | 0 | passed | 0 | 6 | 6 | 235 |
| search-lab/misplace-item-3 | misplace-item | 1 | yes | yes | 0 | passed | 0 | 6 | 6 | 73 |
| search-lab/misplace-item-4 | misplace-item | 1 | yes | yes | 0 | passed | 0 | 5 | 5 | 196 |
| search-lab/misplace-item-5 | misplace-item | 1 | yes | yes | 0 | passed | 0 | 4 | 4 | 167 |
| search-lab/misplace-item-6 | misplace-item | 1 | yes | yes | 0 | passed | 0 | 5 | 5 | 201 |
| search-lab/misplace-item-7 | misplace-item | 1 | yes | yes | 0 | passed | 0 | 6 | 6 | 218 |
| search-lab/misplace-item-8 | misplace-item | 1 | yes | yes | 0 | passed | 0 | 2 | 2 | 31 |
| search-lab/obscure-name-0 | obscure-name | 1 | yes | yes | 0 | passed | 0 | 9 | 9 | 83 |
| search-lab/obscure-name-1 | obscure-name | 1 | yes | yes | 0 | passed | 0 | 9 | 9 | 81 |
| search-lab/obscure-name-2 | obscure-name | 1 | yes | yes | 0 | passed | 0 | 6 | 6 | 61 |
| search-lab/obscure-name-3 | obscure-name | 1 | yes | yes | 0 | passed | 0 | 8 | 8 | 224 |
| search-lab/obscure-name-4 | obscure-name | 1 | yes | yes | 0 | passed | 0 | 8 | 8 | 203 |
| search-lab/obscure-name-5 | obscure-name | 1 | yes | yes | 0 | passed | 0 | 11 | 11 | 343 |
| search-lab/obscure-name-6 | obscure-name | 1 | yes | yes | 0 | passed | 0 | 11 | 11 | 302 |
| search-lab/pair-inline-function-misplace-item-0 | inline-function+misplace-item | 2 | yes | yes | 8 | passed | 3 | 16/65 | 16/20 | 462 |
| search-lab/pair-inline-variable-inline-function-0 | inline-variable+inline-function | 2 | yes | no | - | dominated | - | 17/70 | 17/20 | 932 |
| search-lab/pair-inline-variable-inline-variable-0 | inline-variable+inline-variable | 2 | yes | no | - | dominated | - | 14/58 | 14/20 | 672 |
| search-lab/pair-inline-variable-misplace-item-0 | inline-variable+misplace-item | 2 | yes | no | - | dominated | - | 14/57 | 14/20 | 527 |
| search-lab/pair-misplace-item-misplace-item-0 | misplace-item+misplace-item | 2 | yes | yes | 0 | passed | 0 | 6/16 | 6/9 | 550 |
| search-lab/pair-obscure-name-inline-function-0 | obscure-name+inline-function | 2 | yes | no | - | dominated | - | 16/64 | 16/20 | 827 |
| search-lab/pair-obscure-name-inline-variable-0 | obscure-name+inline-variable | 2 | yes | yes | 2 | passed | 2 | 16/65 | 16/20 | 826 |
| search-lab/pair-obscure-name-misplace-item-0 | obscure-name+misplace-item | 2 | yes | yes | 0 | passed | 0 | 15/61 | 15/20 | 480 |
| search-lab/pair-obscure-name-obscure-name-0 | obscure-name+obscure-name | 2 | yes | yes | 0 | passed | 0 | 17/68 | 17/20 | 698 |
| search-lab/pair-obscure-name-obscure-name-1 | obscure-name+obscure-name | 2 | yes | yes | 0 | passed | 0 | 20/80 | 20/20 | 874 |

`proposed` and `kept` list one number per depth.

### Dropped at

| dropped at | decisions |
|---|---|
| dominated | 4 |

All four misses are depth-2 bests removed by the dominance rule of the pruning before the rank
cut: `pair-obscure-name-inline-function-0` (also the one miss before the change) and the three
pairs with an `inline-variable` that the expander now generates (`inline-variable` plus
`inline-function`, plus `inline-variable`, plus `misplace-item`). In the exhaustive run below
the four bests survive, at beam rank 21 to 25 and final rank 7 to 34: they are not bad candidates,
they are heavy ones (two restores write more bytes than one, and the extract-function restore more
than the others). This is a loss of the pruning, outside the expander (KNOWN LIMITATIONS, untuned
weights).

### Generation (kept beside recall, not in it)

Before this change 11 of 42 decisions did not generate the best: 8 in `search-lab` (every
`inline-variable` plant whose use is inside the initializer of a `let`: 5 at depth 1, `pack`,
`load`, `summary`, `fits`, `run`, and 3 pairs that contain `run`) and the 3 `gen-lab` cases.
After it all 42 generate the best. The causes, each with a case that fails before the change:

| cause | cases on the small bases | fix (`crates/ryo-v2-ra/src/expand.rs`) | test (fails before) |
|---|---|---|---|
| 1. extract-variable read the new variable's name from the first inserted text of rust-analyzer's edit, which is a diff of unpredictable shape. A `let` inserted in front of another `let` lines up on their shared `let `: for `let fitted = fit(needed(v), limit);` the edit replaces `fitted` by `width`, `fit` by `needed`, `needed(v), limit)` by `v)` and inserts `let fitted = fit(width, limit);`, so the name read was `fitted`, the check that the edit writes `let fitted = needed(v);` failed and the candidate was dropped. The seeds were offered; only the name was lost. (The text here before said extract-variable was never proposed for such an expression; it was proposed and dropped.) | the 8 of `search-lab` above | the name is the one of the `let <name> = <expr>;` that applying rust-analyzer's edit adds, read from the edited text (`added_let_name`); exactly one such name, else no candidate | `tests/expand_gen.rs` `extract_variable_restores_a_call_folded_into_a_let_initializer`; `expand::tests::the_added_let_is_read_from_the_edited_text` |
| 2. the same cause for the value of a struct-literal field (`Slot { width: needed(v) }`): the edit replaces `Slot` by `let` and the rest by `width = needed(v);` plus the statement with the shorthand `Slot { width, .. }`, so no inserted text held `let <name>` (CHECK_LEDGER S-6, now removed). The planter writes no such site (it plants only into a call argument). | `gen-lab/inline-variable-field` | as 1, and a field named like the variable (`k: v` with name `k`) is replaced by the shorthand `k`, as rust-analyzer writes it | `tests/expand_gen.rs` `extract_variable_restores_a_record_field_value_as_the_shorthand` |
| 3. move-item's reverse move read the `use` with the comment line above it: the planter moves the item's lines and leaves a plain `//` comment above the re-export, and rust-analyzer attaches that comment to the `use` node, so its text did not start with `[pub ]use`. | `gen-lab/misplace-item-comment-above` | the `use` is read after its leading comments, which stay (`after_leading_comments`; a `use` with an attribute first is still refused) | `tests/expand_gen.rs` `move_item_collapses_a_reexport_under_a_comment` |
| 4. move-item's reverse move required a line end right after the moved item; the planter moves the whole line, and an item whose line ends with a `//` comment has its node end before the comment. | `gen-lab/misplace-item-comment-after` (before: no candidate at all, nothing else in its targets is offered) | the item runs to its line end when the rest of the line is a `//` comment, which moves with it (`line_end_after_comment`) | `tests/expand_gen.rs` `move_item_collapses_an_item_with_a_trailing_comment`; `expand::tests::a_trailing_line_comment_ends_the_item_at_its_line_end` |

Reading the name from the edited text also showed that rust-analyzer names an extracted value
without regard to the locals of the block: for `let width = ..; Slot { width: needed(v), count:
width }` it proposes `width`, and the new `let` would shadow the old binding for `count`, a change
of meaning that compiles. Extract-variable now refuses a name the block mentions outside the
extracted expression (`tests/expand_gen.rs`
`extract_variable_refuses_a_name_the_block_still_reads`). A planted restore never meets it (the
planter's name occurs nowhere else in its function), and every run below gives the same rows with
and without it.

Causes 3 and 4 have one root: the planter's extent of an item is its lines, rust-analyzer's is
its node with the comments it attaches. The expander now follows the lines at both ends; the
forward move still offers only items whose node ends the line (no restore needs more).

New candidates (beam 20, all 42 decisions): 339 children proposed at depth 1 before, 352 after
(+13), and 585 at depth 2 before, 604 after (+19); 570 checks before, 583 after. Each of the 13
depth-1 decisions that gained a child gained exactly one: the restore itself in the 8
`search-lab` and 3 `gen-lab` decisions, the collapse in `misplace-item-comment-after` (counted
among the 3), and in `obscure-name-1` one extract-variable of a call inside a `let` initializer
(a candidate of cause 1 that is not a restore). The depth-2 gains are in 6 pairs (the restores of the
three inline-variable pairs among them; the harness prints no per-candidate kind). The 31
decisions generated before keep their generated, hit, beam rank, fate and final rank. The outer check
(`--outer-check`, 53 s) finds 0 failures in 117 decided candidates and 0 in the 42 first
candidates.

### Generation, second round (the causes of the ryo-rs run)

The ryo-rs run after the fixes above (section "ryo-rs (large run)", subsection "After the four
generation fixes and the containment") left four decisions whose best was never generated, with
three causes. Each cause now has a hand case in `shape-lab` (all its code hand-written) that fails
before the change, and a test of the expander:

| cause | cases on the small bases | fix (`crates/ryo-v2-ra/src/expand.rs`) | test (fails before) |
|---|---|---|---|
| 5. rust-analyzer's syntax tree gives a `///` comment the kind `DOC_COMMENT`, apart from a `//` comment (`COMMENT`); the expander skipped only `COMMENT` to find where an item's visibility or keyword starts. For a documented item that start was the doc line, so the reverse move wrote the item without its doc (the conformance check of move-item refused it, `NotMoved`), and the forward move of a documented private item wrote `pub(crate)` in front of the doc line, which does not compile. The `pub(crate)` the planter writes is not the cause: the reverse move takes the visibility of the `use`. | `shape-lab/misplace-item-doc` (ryo-rs `misplace-item-0` and `-1`) | one predicate `is_comment` (either kind) for the start of an item and for the comments read past above a `use` | `tests/expand_gen.rs` `move_item_collapses_a_documented_item`, `move_item_forward_keeps_the_doc_above_the_visibility`; `expand::tests::a_doc_comment_is_a_comment` |
| 6. the planter folds any one-line initializer into a call argument whose parameter has the variable's name; extract-variable was never offered a reference expression | `shape-lab/inline-variable-ref` (ryo-rs `inline-variable-2`) | extract-variable is also offered a reference (`&e`, `&mut e`) or a literal that is a whole argument of a call or method call; rust-analyzer names it after the parameter | `tests/expand_gen.rs` `extract_variable_restores_a_reference_argument`; `expand::tests::a_reference_or_literal_argument_is_a_variable_shape` |
| 7. the same for a literal | `shape-lab/inline-variable-literal` (ryo-rs pair `obscure-name` plus `inline-variable`, whose file is a bench target that is indexed and expanded like any other) | as 6 | `tests/expand_gen.rs` `extract_variable_restores_a_literal_argument` |

The new candidate class of 6 and 7 is exactly what the two plants need: a path, a `?` expression
or a macro call as an argument, and a reference or literal anywhere but as a whole argument, stay
unoffered (`tests/expand_gen.rs` `extract_variable_offers_no_path_argument` and the shape unit
test). Offering every argument the planter can fold would add a candidate for nearly every argument.

Run at beam 20 (`search-recall run corpus/search`, 45 cases; before: the expander of main fff60ef,
29.7 s, 597 checks; after: 28.9 s without the outer check, 613 checks):

| set | decisions | before: generated / hit / recall@20 / top-3 | after: generated | hit | recall@20 | hit/all | generation | top-3 |
|---|---|---|---|---|---|---|---|---|
| all | 45 | 42 / 38 / 0.90 / 0.80 | 45 | 41 | 0.91 | 0.91 | 1.00 | 0.87 |
| ws shape-lab | 3 | 0 / 0 / - / 0.00 | 3 | 3 | 1.00 | 1.00 | 1.00 | 1.00 |
| kind inline-variable | 17 | 15 / 11 / 0.73 / 0.65 | 17 | 13 | 0.76 | 0.76 | 1.00 | 0.76 |
| kind misplace-item | 17 | 16 / 15 / 0.94 / 0.82 | 17 | 16 | 0.94 | 0.94 | 1.00 | 0.88 |
| depth 1 | 35 | 32 / 32 / 1.00 / 0.89 | 35 | 35 | 1.00 | 1.00 | 1.00 | 0.97 |
| depth 2 | 10 | 10 / 6 / 0.60 / 0.50 | 10 | 6 | 0.60 | 0.60 | 1.00 | 0.50 |

The rows of the other workspaces and kinds are those of section "Counts" (breakage-lab, gen-lab,
search-lab, inline-function, obscure-name: unchanged). The 42 decisions before `shape-lab` keep
their generated, hit, beam rank, fate and final rank; the three `shape-lab` decisions are
generated and hit at beam ranks 1, 1 and 2 (final rank 1 each). Verdict: **adopt** (recall@20 0.91
over 45 generated decisions; 4 misses in 45, the four depth-2 `dominated` of section "Dropped at").

New candidates: on the 42 decisions before `shape-lab`, 352 children at depth 1 before and 367
after (+15: one each in `inline-function-0`, `inline-variable-0`, `obscure-name-0`, two in
`obscure-name-1`, one in each pair but `misplace-item` plus `misplace-item`, two in
`pair-obscure-name-obscure-name-1`), and 604 at depth 2 before, 645 after (+41). Each `shape-lab`
decision gained one child, its restore. No fate moved. On ryo-rs (a verification repository used
as a large sample) the same expansion of the planted states (read once, not a search run) gives
`inline-variable-0` 118 children instead of 71 and `inline-variable-2` 27 instead of 22: the
class grows with the number of reference and literal arguments of a long function, and the
restores of `inline-variable-2` and of the pair are then exposed to the loss of CHECK_LEDGER S-7
like `inline-variable-0` (an expectation; the large run is pending).

`--outer-check` (55.9 s whole run, 26.9 s of it the outer check): 1 of 125 decided candidates
fails, 0 of the 45 first candidates (section "Outer check").


The same 42 decisions at a smaller beam (`search-recall run corpus/search --beam N`), after the
change. The default beam prunes almost nothing at depth 1, so these runs are where the ranking is
exercised. At a narrow beam a depth-2 best can be not generated because its depth-1 parent was
pruned, so `generated` falls with the beam.

| beam | generated | hit | recall | inline-function | inline-variable | misplace-item | obscure-name | depth 1 | depth 2 | dropped at |
|---|---|---|---|---|---|---|---|---|---|---|
| 20 | 42 | 38 | 0.90 | 4/6 | 11/15 | 15/16 | 14/15 | 32/32 | 6/10 | dominated 4 |
| 5 | 42 | 37 | 0.88 | 3/6 | 11/15 | 14/16 | 14/15 | 32/32 | 5/10 | dominated 5 |
| 3 | 41 | 35 | 0.85 | 2/6 | 11/13 | 14/16 | 13/15 | 30/32 | 5/9 | dominated 6 |
| 1 | 40 | 28 | 0.70 | 0/5 | 6/12 | 14/16 | 12/15 | 24/32 | 4/8 | dominated 11, rank 1 |
| 1, before the change | 31 | 23 | 0.74 | 0/5 | 3/5 | 12/13 | 12/15 | 19/24 | 4/7 | dominated 8 |
| 5, second round (45 cases) | 45 | 40 | 0.89 | 3/6 | 13/17 | 15/17 | 14/15 | 35/35 | 5/10 | dominated 5 |
| 3, second round (45 cases) | 44 | 38 | 0.86 | 2/6 | 13/15 | 15/17 | 13/15 | 33/35 | 5/9 | dominated 6 |
| 1, second round (45 cases) | 43 | 28 | 0.65 | 0/5 | 6/14 | 14/17 | 12/15 | 24/35 | 4/8 | dominated 13, rank 2 |

The rows "second round" are the expander of section "Generation, second round" on the 45 cases:
the 42 earlier decisions give the generated and hit of the rows above at each beam, and the three
`shape-lab` decisions add 3 generated and 3 hits at beams 5 and 3 and 3 generated and no hit at
beam 1 (`misplace-item-doc` and `inline-variable-literal` `dominated`, `inline-variable-ref`
`rank`; at beam 20 their bests are at beam rank 2, 1 and 1).

Every miss is `dominated` except one at beam 1 (`gen-lab/inline-variable-field`, `rank`): the
dominance rule, not the score order, is what removes the heavier restores. Recall at beam 3 is
0.85 and at beam 1 it is 0.70 (a `hold`), so H6 still does not rest on the beam being wide: it
holds down to 3, and the weak spot is one stage. The 8 depth-1 bests generated only after the change
(5 in `search-lab`, the 3 of `gen-lab`) give 5 hits and 3 misses at beam 1 (`inline-variable-0`
and `-8` `dominated`, `inline-variable-field` `rank`: their bests are at beam rank 1 or 2 at beam
20).

## Exhaustive (secondary truth)

Second round (45 cases, the expander of section "Generation, second round"): `--exhaustive` 34.8 s,
1002 checks, every generated best survives (45 of 45), the three `shape-lab` bests at final rank 1
each; the table below is of the 42 earlier cases.

`search-recall run corpus/search --exhaustive` (beam 200, no wall limit, 30.5 s, 929 checks; 897
before the change): every generated best survives (42 of 42, recall@200 1.00; before 31 of 31), so
nothing is lost to the HIR check and the misses above are the pruning's. The final ranking by full
reward puts the best at:

| kind (generated decisions) | final rank 0 | final rank 1 to 3 | later |
|---|---|---|---|
| obscure-name (15) | 12 | 2 (ranks 1 and 2) | 1 (rank 9, with an inline-function) |
| misplace-item (16) | 14 | 1 (rank 3, with an inline-function) | 1 (rank 7, with an inline-variable) |
| inline-variable (15) | 6 | 5 (ranks 1, 1, 2, 2 and 2) | 4 (rank 7 with a misplace-item, 32 twice with an inline-variable, 34 with an inline-function) |
| inline-function (6) | 0 | 4 (ranks 1, 2, 3 and 3) | 2 (rank 9 with an obscure-name, 34 with an inline-variable) |

The restore of an inline-function plant is never the top reward, and a depth-2 best that holds an
inline-variable restore ranks 7 to 34: the score weights are untuned (KNOWN LIMITATIONS) and a
bigger edit loses to a smaller one. top-3 is 0.86 overall and 0.33 for inline-function (the count
over measured decisions).

## Noul audit

`MetricJudge` finds no flaw, so the audit is empty: 0 flawed children were checked anyway and 0
of them passed. The numbers are meaningful only with a `ReplayJudge` fixture or a real judge; the
CLI takes `judge: {kind: "replay", file}` and `search_recall` counts both.

## Outer check

`search-recall run corpus/search --outer-check` runs `cargo check --workspace --all-targets` on a
copy of the workspace for each `Decided` candidate (at most 3 per decision) and counts those that
report an error the unedited workspace does not:

| corpus | verifier | decided candidates | failed | rate |
|---|---|---|---|---|
| small bases | before the duplicate-name and field-bounds detectors | 114 | 14 | 0.12 |
| small bases | with them (2026-11-01; 47.8 s wall on 16 cores and 30 GiB, other runs possibly on the machine) | 112 | 0 | 0.00 |
| small bases | second round, 45 cases (2026-11-01; 26.9 s of outer check, same machine) | 125 | 1 | 0.01 |
| ryo-rs | before them | 30 | 2 | 0.07 |
| ryo-rs | with them (2026-11-01, main fff60ef; run 3 of section "After the four generation fixes and the containment", 663,825 ms of outer check on 16 cores and 30 GiB, the machine otherwise running other builds) | 33 | 0 | 0.00 |

Before the detectors all 14 failures were E0255 (a name defined by an item and by a `use` in the
same module), in 11 decisions: the 9 single `misplace-item` decisions and 2 pairs that contain
one. They were the 2nd or 3rd candidate of the decision, never the best: 0 of the 39 best
candidates failed. The failing candidate was move-item's forward shape: it moved the item into
the file that still re-exported it (`pub use crate::plan::NAME;` at the top of
`app/src/lib.rs`, the item appended at its end) and left the `use`. rust-analyzer's diagnostics
passed it and only the outer `cargo check` stopped it. A decision that plants no
`misplace-item` had no failing candidate. (The pairs `inline-function` plus `misplace-item` and
`inline-variable` plus `misplace-item` had none either: their `Decided` candidates did not
include the bad move.)

With the detectors (CHECK_LEDGER Gaps G-B-9, G-B-10, G-B-11) the verifier rejects those moves itself,
and 0 of 112 decided candidates fail. The recall table above (section "Counts") is the same row
for row; every decision keeps its generated, hit, fate and final rank. What moves is the
depth-2 search below a `misplace-item` parent, because the rejected moves are no longer parents:
`pair-inline-function-misplace-item-0` proposes 64 children at depth 2 instead of 57 and keeps
its best at beam rank 8 instead of 7 (final rank 3 both times), `pair-inline-variable-misplace-item-0`
52 instead of 45, `pair-obscure-name-misplace-item-0` 60 instead of 55, and
`pair-misplace-item-misplace-item-0` 16 instead of 18 with 9 kept instead of 10. The decided
candidates are 112 instead of 114: a decision is cut to 3 passing candidates, so where the
rejected move had been one of 3, another passing candidate takes its place if there is one (the
harness prints no per-decision count; the 2 fewer are decisions with no such replacement, an
inference). The run had 564 checks (565 before), median warm check 9 ms.

In the second round the one failure is candidate 2 of `shape-lab/inline-variable-ref`, E0507
(a move out of a borrow) at `src/lib.rs:38`, the line of the folded reference: read from the error
and the line, it is the extract of the nested field without its `&` (`let label = job.spec.label;`),
a shape offered before this round, not the new class. rust-analyzer does not report MIR borrow-check
errors (CHECK_LEDGER Gaps G-B-1, whose list of codes does not name E0507 yet); extract-variable
carries the `Borrow` risk mark, so such a candidate is `AfterOuterCheck`, never auto-applicable
(CHECK_LEDGER M-6). The best (candidate 0) passes.

On ryo-rs (a verification repository used as a large sample; run 3 of section "ryo-rs (large
run)", before the detectors) 2 of 30 decided candidates failed, in 10 decisions
(`inline-variable-0` had no candidate), both in `misplace-item` decisions: candidate 0 of
`misplace-item-0` reports E0255 at `crates/ryo-agent/src/decide.rs:147`, candidate 1 of
`misplace-item-2` reports E0204 (a `Copy` impl on a type with a field that is not `Copy`) at
`crates/ryo-agent/src/decide.rs:150` (paths inside ryo-rs). The harness counts 1 of the 10
first candidates of a decision as failed: candidate 0 of `misplace-item-0`, a decision whose
planted best was not generated. Both codes are now detected (CHECK_LEDGER Gaps G-B-9 and G-B-10).
With the detectors (run 3 at main fff60ef) 0 of 33 decided candidates fail in the 11 decisions, and
0 of the 11 first candidates; `inline-variable-0` now has candidates, so all 11 decisions are
decided.

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

Generation is 6 of 11 (0.55; the small bases gave 0.79 then). The five decisions whose best is
never generated are `inline-variable-0` (no candidate at all, fate `no-candidates`),
`inline-variable-2`, `misplace-item-0`, `misplace-item-1` and the pair `obscure-name` plus
`inline-variable`. The causes found on the small bases since (section "Generation") are fixed;
which cause each of the five had was read on the run after them (subsection "After the four
generation fixes and the containment"; CHECK_LEDGER S-4, now removed).

Run 2 (`--exhaustive --limit 10`: beam 200, no wall limit, the first 10 cases, which leaves out
`pair-obscure-name-obscure-name-0`; 466,780 ms, 304 checks): 5 of the 10 decisions generate the
best and all 5 survive (recall@200 1.00, generation 0.50, top-3 0.30, verdict insufficient). The
9 depth-1 decisions have the same generated, beam rank and final rank as in run 1. The pair
`obscure-name` plus `inline-variable` keeps every child at depth 1 (44 of 44, and 169 of 175 at
depth 2) and still does not generate its best, so that miss is the expander's, not the beam's.

Each of the three runs hit one panic inside rust-analyzer's syntax factory, raised from the
expander, and went on past it; the harness did not count it then, so the tables above do not show
it. The producer is extract-variable: rust-analyzer names the variable after the whole text of
the struct-literal field whose value it extracts, and a field behind `#[cfg(..)]` makes that text
no identifier. The panic then ended the whole expansion of that parent state with no child.
In the run after the containment no panic is contained and `inline-variable-0`, the one decision
with no candidate before, has 71 (subsection "After the four generation fixes and the
containment"), so that parent was most likely its planted state (an inference from the two runs,
not traced). The expander now
refuses such a value, contains a panic per call of one edit kind, and records it on the session.
`search-recall run` prints the line `contained panics: <n> detector, <n> expander`, and the oracle
fails on any contained panic.

### After the four generation fixes and the containment (2026-11-01, main fff60ef)

The same 11 cases, run again on 2026-11-01 at main fff60ef (the expander of section "Generation"
and the containment of the panic above), on 16 cores and 30 GiB with the machine otherwise running
other builds, two runs one after the other: run 1 at beam 20, run 3 the same with `--outer-check`
(no exhaustive run). Run 3 gives the same table and the same beam rank, fate and final rank for
every decision as run 1; its outer check is in section "Outer check", its trigger lines in section
"Perf triggers". The numbers below are from run 1 (380,928 ms, 206 checks).

| set | decisions | skipped | before: generated / hit / recall@20 / top-3 | after: generated | hit | recall@20 | hit/all | generation | top-3 |
|---|---|---|---|---|---|---|---|---|---|
| all | 11 | 0 | 6 / 6 / 1.00 / 0.27 | 7 | 6 | 0.86 | 0.55 | 0.64 | 0.18 |
| kind inline-variable | 4 | 0 | 1 / 1 / 1.00 / 0.25 | 2 | 1 | 0.50 | 0.25 | 0.50 | 0.00 |
| kind misplace-item | 3 | 0 | 1 / 1 / 1.00 / 0.33 | 1 | 1 | 1.00 | 0.33 | 0.33 | 0.33 |
| kind obscure-name | 6 | 0 | 5 / 5 / 1.00 / 0.17 | 5 | 5 | 1.00 | 0.83 | 0.83 | 0.17 |
| depth 1 | 9 | 0 | 5 / 5 / 1.00 / 0.33 | 6 | 5 | 0.83 | 0.56 | 0.67 | 0.22 |
| depth 2 | 2 | 0 | 1 / 1 / 1.00 / 0.00 | 1 | 1 | 1.00 | 0.50 | 0.50 | 0.00 |

Verdict: **insufficient** (recall@20 0.86 over 7 generated decisions, fewer than 30). Contained
panics: 0 detector, 0 expander, in both runs (each run before hit one).

| decision | generated | hit | beam rank | fate | final rank | proposed | kept | wall ms |
|---|---|---|---|---|---|---|---|---|
| ryo-rs/inline-variable-0 | yes | no | - | dominated | - | 71 | 20 | 38352 |
| ryo-rs/inline-variable-1 | yes | yes | 4 | passed | 4 | 8 | 8 | 9186 |
| ryo-rs/inline-variable-2 | no | no | - | - | - | 22 | 20 | 6837 |
| ryo-rs/misplace-item-0 | no | no | - | - | - | 5 | 5 | 4445 |
| ryo-rs/misplace-item-1 | no | no | - | - | - | 3 | 3 | 3636 |
| ryo-rs/misplace-item-2 | yes | yes | 0 | passed | 0 | 7 | 7 | 11156 |
| ryo-rs/obscure-name-0 | yes | yes | 3 | passed | 3 | 20 | 20 | 12319 |
| ryo-rs/obscure-name-1 | yes | yes | 2 | passed | 5 | 17 | 17 | 40442 |
| ryo-rs/obscure-name-2 | yes | yes | 1 | passed | 1 | 15 | 15 | 31674 |
| ryo-rs/pair-obscure-name-inline-variable-0 | no | no | - | - | - | 47/187 | 20/20 | 63056 |
| ryo-rs/pair-obscure-name-obscure-name-0 | yes | yes | 13 | passed | 12 | 33/131 | 20/20 | 69300 |

`inline-variable-0` now has candidates (71; before: none, the panic of CHECK_LEDGER S-3 was in its
planted state) and its best is generated, but the pruning drops it at depth 1. The four decisions
still not generated have three causes, each read from the expander's output on the planted state
of ryo-rs (the expansion of each planted state over the case's targets, every Blueprint applied and
compared with the clean text) and each now with a small-base case (section "Generation, second
round"):

| decision | what the plant does | cause |
|---|---|---|
| `misplace-item-0`, `misplace-item-1` | moves a private item that has a `///` doc line to another module file of the crate, as `pub(crate)`, and leaves a `use` of it | rust-analyzer's syntax tree gives a `///` comment its own kind (`DOC_COMMENT`), apart from a `//` comment (`COMMENT`), and the expander skipped only the second when it looked for the start of the item's visibility or keyword. For a documented item that start was the doc line, so the reverse move wrote the item without its doc and the conformance check of move-item refused it (`NotMoved`): no reverse move was proposed. The visibility is not the cause: the reverse move writes the item with the visibility of the `use` (none here), as it did for the `gen-lab` case with `pub(crate)`. |
| `inline-variable-2` | folds a reference to a nested field (`&a.b.c`) into a call argument | extract-variable was never offered a reference expression; only the field expression inside it was, and that extract (no `&`) is not the restore. |
| `pair-obscure-name-inline-variable-0` | renames a local in one file and folds a literal into a call argument in a bench target (`benches/`) | the bench file is indexed and expanded (its renames and extracts are among the 47 children); extract-variable was never offered a literal. The obscure-name half is generated. |

Why `inline-variable-0` is dropped (from the trace of the same expansion, scored with
`SyntaxAssessor` and the rule of `prune`): its target function is long (about 200 lines), so its
71 children fill the beam of 20 already at depth 1, where the small bases never fill it (section
"Counts", reservation 1). The restore writes one `let` line into a function beyond the 25 lines of
the long-function cost, so its `clean` is -1, as it is for every extract-variable in that function,
while a rename keeps `clean` at 0; the extensibility term is 0 for every child (CHECK_LEDGER S-2).
17 children dominate the restore (at least its `clean`, at most its 161 edit bytes and its one file,
better in one), 9 of the 71 are dominated by none. The 9 come first and the rest go by score, where
the -1 of `clean` weighs 10 and the bytes 1 per 100: the many renames with `clean` 0 outrank it and
it falls past the beam. This is the loss of CHECK_LEDGER S-7 (a heavier restore loses to lighter
siblings under the untuned weights), here at depth 1 and through the long-function cost.

## Perf triggers (decision D4)

The table is filled by every run; the small bases cannot fire trigger 1 to 3 (they are tiny). The
ryo-rs columns are run 1 of section "ryo-rs (large run)" (ryo-rs is a verification repository used
as a large sample; 16 cores, 30 GiB): "before" the first run (the machine otherwise idle), "after"
the run at main fff60ef of its subsection "After the four generation fixes and the containment"
(the machine otherwise running other builds).

| trigger | limit | small bases | ryo-rs before | ryo-rs after |
|---|---|---|---|---|
| 1 median decision | 300,000 ms | 138 ms (not fired) | 8,986 ms (not fired) | 12,319 ms (not fired) |
| 2 median warm check (Affected) | 5,000 ms | 5 ms over 565 checks (not fired) | 1,476 ms over 172 checks (not fired) | 1,187 ms over 206 checks (not fired) |
| 3 RSS after 1,000 checks, or check time x1.5 | 8 GiB, or 1,500 permille of the first 100 checks | 595,024 kB after 565 checks; both halves not measured: fewer than 1,000 checks | 5,653,460 kB after 172 checks; both halves not measured: fewer than 1,000 checks | 5,501,608 kB after 206 checks; both halves not measured: fewer than 1,000 checks |
| 4 one recall run without its outer check, default budget | 600,000 ms | 14,556 ms (not fired) | 360,538 ms (not fired) | 380,928 ms (not fired) |

Run 3 after (`--outer-check`, main fff60ef): 14,288 ms, 1,092 ms over 206 checks, 5,633,336 kB
after 206 checks (not measured), and 389,913 ms without its outer check (outer check 663,825 ms,
whole run 1,053,738 ms), not fired: the harness now prints trigger 4 as D4 restates it, and run 3
is the first run with `--outer-check` judged that way by the harness itself. The outer check of 33
candidates took about 20 s each (663,825 / 33, derived).

The other two ryo-rs runs: run 2 (`--exhaustive --limit 10`) 9,810 ms, 847 ms over 304 checks,
5,701,156 kB after 304 checks (both halves of trigger 3 not measured) and 466,780 ms, none fired;
run 3 (`--outer-check`) 10,149 ms, 1,265 ms over 172 checks, 5,645,592 kB after 172 checks (not
measured) and 983,033 ms for the whole run, which the harness of that day printed as trigger 4
fired. Triggers 1 and 2 fired in no run, and trigger 3 is not measured on ryo-rs: no run reached
1,000 checks. Under trigger 4 as restated in D4 (2026-11-01), run 2 is not measured (its budget
is beyond the default) and run 3 is about 348,068 ms without its outer check (not fired; derived
below, the harness now prints it measured).

Trigger 4 in run 3: the decisions of run 3 take about as long as those of run 1 (their wall times
sum to 277,368 ms against 289,838 ms), and the rest of the run is 705,665 ms against 70,700 ms
in run 1. The difference, 634,965 ms, is the outer `cargo check` of the 30 decided candidates
(about 21 s per candidate, derived from these sums, not measured per candidate), which runs after
each search and outside it. None of the steps (a) to (d) of D4's work order would shorten the
outer check. D4 therefore restates trigger 4 (2026-11-01): the time judged leaves the outer check
out, and only runs within the default budget are judged. `search-recall run` now times the outer
check apart and prints the trigger 4 line as `trigger 4, one run without the outer check >
600000 ms: <search> ms (outer check <outer> ms, whole run <whole> ms) (<status>)`; run 3 reads
983,033 - 634,965 = 348,068 ms there, not fired.

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
  whose 11 decisions on ryo-rs (6 compared, 7 in the run at main fff60ef) decide nothing either.
- **The expander offers extract-variable only for some expression shapes**: calls, method calls,
  index, binary and nested field expressions, and a reference or literal that is a whole argument
  of a call (section "Generation, second round"). The planter folds any one-line initializer into
  a call argument, so a plant whose initializer is another shape (`x?`, `a.b`, a macro call, a
  path) is never restored. No small-base case has such a plant; on ryo-rs (a verification
  repository used as a large sample) the two of its 11 cases with another shape were a reference
  and a literal.
