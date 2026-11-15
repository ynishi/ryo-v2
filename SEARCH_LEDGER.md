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
items as places, no root Blueprints, and the depth of the best (1 for one plant, 2 for two plants
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
it and the guard that fails when the value is broken. D1 to D4 were accepted 2026-10-31; D4's
trigger 4 was restated 2026-11-01 after the first ryo-rs runs (its "revisit when"); D5, D6 and D8
were accepted 2026-11-01. D5's sentence on dispositions and its guard
`unseen_marks_never_change_disposition` were restated by D6 (the disposition they guarded is
gone). D7, accepted 2026-11-01, restated D5's value: which classes a candidate names. D2, D3 and D4 were restated 2026-11-01 by D9: CONCEPT.md makes stacked writes the first condition of the execution model, so D2's sampling counts writes, D3's one fork per session is a fork whose writes stack, and D4 no longer counts parent-to-child writes as perf work; their measurements stand. D4's rejected option of parent-to-child deltas is taken up by D9 without a new measurement, the one exception to the rule below that a rejected option is not taken up again without a new measurement: CONCEPT.md decides it, and where a ledger decision disagrees with CONCEPT.md the decision is what is wrong (CONCEPT.md, and DEVELOPMENT.md, "Documentation"). D10 was accepted 2026-11-02. D1, D4, D7 and D9 were restated 2026-11-02 by D10: the search verifies its candidates on one stacked fork, a child written on top of its parent, its expander reads that fork, and every verdict is formed by one function whose one-level case is a check; their measurements stand. D11 was accepted 2026-11-03. D3 was restated 2026-11-03 by D11: forks with several live at once are layers of the Engine in one process, not copies of the process made by the operating system; its measurement stands. D2 gained 2026-11-03 the sentence on a revert audit that cannot run, and D4, D9 and D10 gained the ryo-rs measurement of section Bench, "Stacked against rebuilt", in place of their "ryo-rs pending". D11 gained 2026-11-04 the Engine's reads under a deep chain and the decision to keep the `LOW` stamp walk, with its ceiling. D11 gained 2026-11-05 stage 3b: N live forks in parallel on the Engine's job pool behind the `Forks` port; D3's "until the Engine is behind the port" was restated by it the same day; its measurement stands. D1, D2, D3, D4, D7, D9, D10 and D11 were restated 2026-11-15 by D11 stage 4: the search runs on N live forks through the `Forks` port, each verified node owning its Layer, so D10's re-path of one stacked fork (`repath`) is gone; every fork of a session is layers of the Engine over a frozen base, the undo-log backend is deleted and the flat reference check is the one write of the base left (D2); the `Stacked` port is deleted with the adapter's `RaStack` and `tests/stack.rs`, so the guards that named them now name the clauses of `Forks`, of its adapter and of the check, and the tests of `tests/jobs.rs` and `tests/check.rs` that carry their assertions; a guard with no `Forks` counterpart went with the port, and its row says so. Their measurements stand; D11 gained the gate of the deletion (the 45 rows of the small bases unchanged) and two deviations from the plan of stage 4 (the report has no `build` hash key; the `--engine` flags were removed instead of kept as no-ops). A row is changed, with a new measurement, when its "revisit when" is met; a rejected option is not taken up
again without a new measurement. The measurements are from section "Bench" (assert off,
Affected scope, warm session; 12 threads, 30 GiB). ryo-rs is a verification repository used as a
large sample.

| id | decision (current value) | why it holds today (measurement, date) | guard (constant / test / contract clause) | revisit when | rejected options |
|---|---|---|---|---|---|
| D1 time budget of one decision | One decision is one `search_decision` call (expand, prune, verify the survivors as jobs of the search's forks, a child on its parent's Layer (D10, D11), repeat to a depth, rank). Wall time 300 s soft and 360 s hard; 360 s = 20 x 3 x 6 s, the product of `beam`, `max_depth` and `per_candidate_ms` of `DEFAULT_SEARCH_BUDGET`. Soft: no new depth starts once 300 s have passed (`SearchReport.soft_stopped`); hard: the search stops at once (`SearchReport.budget_exhausted`). One check is cut off after 6 s (`per_candidate_ms`, `CheckError::TimedOut`); the candidate is recorded as `Blocking::BudgetExhausted { limit_ms }` and the search goes on. Expand plus static metrics: 10 s per depth (200 candidates x 50 ms). Judge: one call per depth, 30 s; the metric judge takes 0. At the hard limit the result is `Decided` with what has been verified so far; with nothing passed it is `Failed` with `budget_exhausted: true`. The expand and judge allowances are recorded, not enforced, because the metric judge and the syntactic assessor are far below them. Wall time is read from the `Clock` port, so tests drive the budget with a fake clock. | 2026-10-31, section "Bench": one check takes 0.3 to 4.3 s, one revert 0.04 to 0.36 s, check plus revert 2 to 3 s at the median. At most 20 candidates per depth over at most 3 depths is at most 60 checks, which is 120 to 180 s at the median, so 300 s leaves the second half of the budget for expansion, the judge and the slow tail, and 360 s is derived from the knobs, not chosen separately. 6 s is a little above the measured 4.3 s maximum; a check beyond it is a candidate CONCEPT.md Core 4 (5 s or less per candidate) already rejects. A caller who waits 6 minutes gets the verified candidates, not an error. Consequence: a base where most checks exceed 6 s yields `Failed`, the signal to start the perf work of D4. 2026-11-02, restated by D10: the time in the checks of a depth (`DepthStats.check_ms`) now holds the writes and pops that bring the fork to each child's parent; the `check_ms` of one check (`Event::Checked`) is the child's write and diagnosis (`Stacked::diagnose` C7); the limits are unchanged. 2026-11-15, restated by D11 stage 4 (section Bench, "The search on N forks (D11, stage 4)"): a child is a job of the `Forks` port on its parent's Layer, so `DepthStats.check_ms` holds no re-path; the `check_ms` of a child's `Event::Checked` is its job's write and diagnosis, which no clause of `Forks` states and no test checks (`Stacked::diagnose` C7 and its test went with the port). The hard limit is read before each check job is submitted, and a job already submitted runs to its own cut-off (`search_decision` C5); the start state's job is no candidate and has no cut-off (`Limit::None`, `Forks` C11); the limits are unchanged. | `DEFAULT_SEARCH_BUDGET` (beam 20, `max_expand` 200, `beam_parents` 4, `max_depth` 3, `per_candidate_ms` 6,000, `soft_ms` 300,000, `hard_ms` 360,000), `EXPAND_DEPTH_MS` = 10,000, `JUDGE_DEPTH_MS` = 30,000; contract clause C1 of `DEFAULT_SEARCH_BUDGET`, test `app::search::tests::default_search_budget_is_the_decided_numbers` (pins every value, including the allowances); behaviour: `soft_budget_stops_after_the_depth`, `budget_exhausted_returns_verified_so_far`, `hard_budget_with_nothing_verified_is_failed_exhausted`, `check_timeout_is_budget_exhausted_blocking` (core), `tests/timeout.rs check_past_timeout_is_timed_out_and_session_usable` (ra). Restated 2026-11-15 (D11 stage 4): C5 of `search_decision` (`the_hard_budget_is_checked_before_each_submit`), C11 and C17 of `Forks` (`the_start_job_is_not_cut_off` (core), `tests/forks.rs a_start_job_is_not_cut_off`, `tests/jobs.rs a_cut_off_job_keeps_its_parent_usable`), C1 of `with_check_timeout`. The allowances have no behaviour guard: they are not enforced; nor has the `check_ms` of a job (above). | The median decision on the ryo-rs corpus exceeds 300 s (D4, trigger 1); the measured slowest check exceeds 6 s on a base the user cares about; a real judge (an LLM) is added, whose 30 s per depth becomes an enforced limit. | No budget (the M3 state): one decision's duration cannot be stated, so CONCEPT.md Core 4 cannot be checked at all. A candidate-count budget only (beam times depth): on a large base one check takes more than 4 s as a matter of course, so the same count means 2 minutes or 10 and the time cannot be read from it. |
| D2 revert assertion in production | `Verifier::check` is the one-job case of the session's forks (D9, D11): it writes the edits into a layer over the frozen base, diagnoses and drops the layer, so it writes nothing to revert (`RaAnalyzer::check` C1); a job of `Forks` is one write as a check is. On every 50th write (`RaConfig.assert_revert_every = Some(50)`; a check or a job is one write) the end of the write's level checks the isolation of the base (I2): no write the frozen base refused, and the base's fingerprint kept (for a job at `close`); a check re-asserts no revert (`RaAnalyzer::check` C2, C4). The revert audit runs once at the end of every decision, after the forks are closed and before the Resolution is returned (`RevertAudit::audit_revert`, over the paths of every write since the last audit, every job's included): it checks I2 first, then diagnoses those paths again and compares them with the baseline. The one write of the base left is the flat reference check (`RaAnalyzer::reference_check`, the flat side of the differential tests): it writes the edits with rust-analyzer's setters, diagnoses, writes the base texts back, compares the database text of each file it modified or deleted with its base text (cheap, O(touched bytes)) and, with `assert_revert`, diagnoses the scope of the edited paths again and compares it with the baseline. The bench and the oracle keep `Some(1)`. A mismatch poisons the session (`CheckError::RevertMismatch`, then `Poisoned`), and so does a write of the base the Engine finds (I2). Every failure of the reference check's write-back poisons the session (a mismatch, a database write that panics, a re-assert that cannot run), so a failure stops it at once: from then on nothing more is written to the session's database, while its reads still describe the base. The caller reloads a poisoned session, which costs about 40 s (warm load 3.2 s plus baseline 35 s), outside the budget of D1 and recorded as evidence. A mismatch found at the audit rejects the whole decision (`SearchError::RevertAudit`), not only the last check, because it cannot say when the drift began. `RevertAudit` is a port beside `Verifier`; the fake verifier of the core tests implements it. A revert audit that cannot run returns that failure and keeps the paths it was to audit: the record is kept until a diagnosis of them runs to its end, clean or not (`RevertAudit` C3, 2026-11-03). | 2026-10-31, section "Bench": the re-assert costs 1.4 to 2.1 s, which doubles the cost of a check, so every check cannot pay it in a search of 60. Every 50 checks it is 30 to 40 ms per check, 1 to 2 % of a check. The end-of-decision audit costs one re-assert (about 2 s of 360 s) and means a `Decided` result has always been followed by a clean diagnostic comparison of the session it came from. The fingerprint catches text drift (review finding V5) on every check; what it cannot see is the analyzer's derived state drifting while the text is right, and sampling plus the final audit bounds how long such a drift can go unnoticed. 2026-11-01, restated by D9: the period counts writes, because a write is the unit a revert undoes; the cost per check above is the cost per write of one level. A sampled level above the first also pays one diagnosis of its scope before its write; that is not measured. 2026-11-15, restated by D11 stage 4: every fork of a session, a check's included, is layers over the frozen base, and the undo log is deleted, so a check has no revert to compare: its sample checks the isolation of the base, and the text comparison and the re-assert stay where the base is written, in the flat reference check, which is tied to a fresh load (`reference_check` C2). The measurements above are of the undo log's re-assert and stand as its record; I2 held in every stage-4 run (Table F3). | `ASSERT_REVERT_EVERY` = 50, `RaConfig.assert_revert_every` C1; contract clauses C1, C2 and C4 of `RaAnalyzer::check` and C1, C2 and C4 of `RevertAudit for RaAnalyzer`; C7 of `search_decision`; tests `tests/verifier.rs check_leaves_text_and_baseline_at_base`, `tests/verifier.rs assert_revert_every_samples_the_diagnostic_assert`, `tests/verifier.rs a_sampled_check_reasserts_no_revert`, `tests/timeout.rs audit_revert_is_ok_after_checks_and_refuses_poisoned`, core `revert_audit_runs_once_at_end`; the value 50 is pinned by `default_search_budget_is_the_decided_numbers`. The sampling period is a constant; changing it needs a new bench of the poison rate. Restated 2026-11-01 (D9) and 2026-11-15 (D11 stage 4): C2 of `RaAnalyzer::check` reads "every n-th write, a check being one write"; C9 of `RaAnalyzer::check` and C11 of `impl Forks for RaAnalyzer` record the paths of every check and every job, a refused one's included, for the audit; C7 of `Forks` poisons the session on a write the frozen base refused; C1 and C3 of `RaAnalyzer::reference_check` compare the texts and re-assert the level of the one write of the base; tests `tests/check.rs writes_count_for_the_sampled_isolation`, `check_writes_are_audited`, `the_flat_reference_reasserts_its_level`, `a_revert_that_cannot_be_confirmed_poisons_the_session`, `a_database_write_that_panics_poisons_the_session`, `text_is_the_base_after_a_poisoned_check`, `tests/jobs.rs a_jobs_paths_join_the_revert_audit`, `tests/forks.rs a_refused_base_write_poisons_and_withdraws`. The clauses of the deleted `Stacked` port and its adapter (a pop's and a release's text comparison and re-assert against the level below) went with it: no layer is reverted. 2026-11-03: C3 of `RevertAudit for RaAnalyzer`; test `tests/timeout.rs a_failed_audit_keeps_its_paths` (an audit made to fail by the hidden fault hook `with_audit_fault` keeps both recorded paths, `pending_audit`; the next audit diagnoses them and forgets them). | A poison (revert mismatch) is seen in production use: lower the period or audit every depth; the re-assert gets cheaper (item-level scope, D4 order step 1), which allows a shorter period; the check time grows by 1.5 times over a long run (D4, trigger 3); a stacked search (D9) shows a poison. | Re-assert on every check: the cost of a check doubles (the 3 to 6 s of the bench) and the 6 s cut-off of D1 would fire on healthy checks. Fingerprint only: if salsa state drifts without the text changing nothing detects it and `Decided` is silently wrong, which is the failure the engine exists to avoid. |
| D3 parallelism and memory | One decision uses one session (one loaded rust-analyzer database). Its candidates are jobs of the session's forks (D11, stage 4): forks with several live at once are layers of the Engine over the frozen base in one process, and the search keeps at most `Forks::width` jobs, checks and expansions, submitted and not yet received (the host and the oracle ask `SEARCH_FORKS` = 4); siblings are submitted one after another and the outcomes are consumed in submission order, so a search with no wall-clock effect gives the report it gives at width 1 (`search_decision` C16). The threads (`RaConfig.threads`, default the available parallelism, T) are shared by D11's budget: `W = min(N, T_w)` jobs at once, a job's diagnostic threads `t = max(1, T_w / W)` splitting the files of its scope. One fork of the adapter is live per session (`Forks`, or an `Analyzer` fork): while it is, anything else that needs the session's base is `ForkInUse`, and so is a Layer of another session. Its writes stack (D9). CONCEPT.md puts the number of live forks at 4 to 16 (a hypothesis); the window is D11's decision, with its own measurement. More sessions are started only after a D4 trigger fires, and then at most `N = max(1, floor((RAM_GB - 8) / 7))` (`RESERVED_RAM_GB = 8`, `SESSION_RAM_GB = 7`, a session's base with its search's Layers; N = 3 on 30 GB), stated by `max_sessions(ram_gb)` in `ryo-v2-core`. The `Verifier` port keeps `&mut Session`: one check at a time per session, enforced by the type; `Forks` takes the session mutably for every call that changes it, so one thread owns the session while the jobs run on the workers. A caller that wants two decisions at once starts two processes and owns the memory budget. `max_sessions` exists as a function and a test, not as a feature. | 2026-10-31, section "Bench" (30 GiB, 12 threads): one session of the base of ryo-rs takes about 5 GB and 60 repeated runs grew it by about 0.75 GB, so 6 GB per session; a second `AnalysisHost` doubles the resident set size; salsa gives no writable fork, so edits are applied to the one database and reverted. The diagnostics of one check already run on all cores by file, so a second session adds parallelism only for the cores one check leaves idle (an Amdahl bound, not measured). The memory cap decides first: 8 GB stay free for the rest of the machine, so 3 sessions on 30 GB and a best case of 3 times. Serial checks keep the search deterministic and the revert audit simple (one session, one history); races and poisoning with several sessions are a class of failure to take on only when the serial path is correct and too slow. 2026-11-01, restated by D9: the reason against several forks in one session (salsa has no writable fork; a second `AnalysisHost` doubles the resident set) still holds inside one process. CONCEPT.md's model asks for N live forks whose writes stack, and the route it leaves is a writable fork of the analysed state in memory (D11). 2026-11-03, restated by D11: several forks live at once are layers of one frozen database in one process, not copies of the process made by the operating system (rejected, D11); the measurement above (5 GB per session, a second `AnalysisHost` doubles the resident set) stands, and D11 reads `RESERVED_RAM_GB` and `SESSION_RAM_GB` as GiB. D11's stage 1 bounds a layer's memory from below (section Bench, "Memory composition (engine probe, D11)"). 2026-11-05, `SESSION_RAM_GB` restated by D11 stage 4 (section Bench, "The search on N forks (D11, stage 4)"; 12 cores, 30.27 GiB): a session is its base plus its search's Layers; the peak RSS of one stage-4 session over the ryo-rs corpus at W = 4 is 5.65 GiB, so ceil(5.65) plus the growth margin above (0.75 GB, rounded up) gives 7; `max_sessions(30)` stays 3. 2026-11-15, restated by D11 stage 4 (the same section; 12 logical cores, 6 physical × 2 SMT, 30.27 GiB): the search runs `SEARCH_FORKS` = 4 jobs at once (Table F1: a check at W = 4 costs about what it costs alone); serial verification on one fork per session was the rule until then. The determinism serial checks gave is kept by consuming the outcomes in submission order (W4-search and C4-4 met, Table F3), and the revert audit by one session and one base no fork writes (D2). The measurement above stands. | `max_sessions`, `SESSION_RAM_GB` = 7, `RESERVED_RAM_GB` = 8; contract clause C1 of `max_sessions`, test `app::search::tests::max_sessions_follows_decision_d3` (30 gives 3, 0 and 8 and 14 and 20 give 1, 22 gives 2, 64 gives 8); the two constants are pinned by `default_search_budget_is_the_decided_numbers`; the `&mut Session` of the `Verifier` port is enforced by the type. Restated 2026-11-01 (D9) and 2026-11-15 (D11 stage 4): C5 and C6 of `Forks`, C9 of `impl Forks for RaAnalyzer`, C4 and C5 of `Analyzer for RaAnalyzer`; tests `tests/forks.rs session_operations_refuse_while_forks_are_open`, `a_layer_of_another_session_is_refused`, `tests/jobs.rs a_base_write_with_a_live_layer_is_fork_in_use`, `tests/check.rs session_operations_refuse_while_an_analyzer_fork_is_live` and `an_analyzer_ticket_of_another_session_is_refused`. The window: `SEARCH_FORKS` pinned by `default_search_budget_is_the_decided_numbers`; C16 of `search_decision` (`parallel_search_equals_serial`, `the_window_never_holds_more_than_w`); C4 of `Forks` (`tests/forks.rs job_threads_times_jobs_fit_the_budget`). | A D4 trigger fires (the median decision exceeds 300 s, or the median warm check exceeds 5 s) after the cheaper steps of its order are done; the backend offers a cheap, writable fork; the machine has less than 15 GB (the formula gives 1; one session needs 7 GB to be safe); a stage gate of the Engine fails (D11). | N sessions now: the memory caps the gain at 3 times on a 30 GB machine, and the race and poison handling would be built before the serial path is shown correct and too slow. Several forks in one session: the backend (salsa) has no writable fork, so this is not buildable without replacing the backend. A fork rebuilt from the base for every check (the rule until 2026-11-01): against CONCEPT.md's first condition of the model. Copies of the process by the operating system (D11). |
| D4 when to start perf work | No perf work during M4; the harness only measures. Stacked writes (D9) are not perf work: CONCEPT.md makes them the first condition of the model. A perf task is opened when any of these happens on the ryo-rs decision corpus (the Perf triggers section below shows their status): (1) the median wall time of a decision exceeds 300 s (the soft limit of D1); (2) the median warm check in Affected scope exceeds 5 s (CONCEPT.md Core 4); (3) after 1,000 checks the resident set size, read after a decision's `close` (between decisions), exceeds one session's memory, `SESSION_RAM_GB` (D3; 8 GB until 2026-11-05), or the check time has grown by 1.5 times (review finding V4); (4) one recall run of the planted corpus takes more than 10 minutes without its outer check: the time judged is the wall time of one `search-recall run` less the time of the oracle's outer `cargo check` (the baseline of each workspace and every decided candidate under `--outer-check`), session loads included; the run judged is any run whose search budget is nowhere larger than `DEFAULT_SEARCH_BUDGET` (a narrower beam is judged, `--exhaustive` or a wider beam is not measured), on any corpus and any machine, the ledger naming the machine beside the number. Work order when a trigger fires, cheapest and most effective first: (a) item-level scope: diagnose only the files that reference the changed item (434 of 504 files today; tens expected, an estimate), with the missed-error risk measured by the oracle; (b) order siblings so that checks that touch the same files run together; (c) parent-to-child deltas: taken out 2026-11-01, built as D9; (d) N sessions (D3). The letters are kept, so that "D4 order step 1" in D2 and the "(a) to (d)" of the trigger-4 sentence still resolve. Each step follows the usual profiling procedure: save a baseline, take a flame graph, change the hot path only, bench again. The constants live in `ryo-v2-core::app::search` so the harness and this row cannot disagree. The ryo-rs run is outside the default test suite, so trigger 4 is judged on the small bases and on each recorded large run alike. | 2026-10-31: the numbers behind D1 to D3 come from small and medium bases and the decision corpus of ryo-rs does not exist yet at scale; perf work started without a number to move is guesswork, and perf work never started leaves the budget of D1 unwatched. Item-level scope attacks the largest factor (files diagnosed per check); deltas only remove the revert, which is 0.04 to 0.36 s of a 0.3 to 4.3 s check, about 10 %. The triggers are the limits D1 and CONCEPT.md Core 4 state, so each says "a promise we made is broken" and not "something could be faster". 2026-11-01, trigger 4 restated (section "Perf triggers"; ryo-rs, a verification repository used as a large sample, 16 cores, 30 GiB): run 1 (beam 20) took 360,538 ms; run 3, the same with `--outer-check`, took 983,033 ms, of which about 635 s (634,965 ms, derived from the sums of the decisions' wall times, about 21 s per decided candidate) is the outer check, which runs after each search and outside it. Its decisions took 277,368 ms against 289,838 ms in run 1, so the search was no slower; read as the whole run, trigger 4 would open a perf task that none of the steps (a) to (d) can serve, which is "something is slow", not "a promise is broken". The outer check is the oracle's second opinion of the verifier, not a cost the user of a decision pays. An exhaustive run lifts the wall limits of D1 on purpose, so its time breaks no promise; a narrower beam can only be faster, so a fire there is a fire at the default too. 2026-11-01, restated by D9: the parent-to-child delta (step (c) and the "optimise now" option below) is the first condition of CONCEPT.md's execution model, and a ledger decision that disagrees with CONCEPT.md is what is wrong. The risk named against it, a second state to verify, is guarded by the differential contract: a stack diagnoses as its flat composition. The 10 % above is the saving of the revert alone in a flow that rebuilds every child from the base; what stacking saves is measured on the small bases and on ryo-rs in section Bench, "Stacked against rebuilt" (D10). On ryo-rs (2026-11-03; 12 threads, 30.27 GiB, otherwise idle; release builds) the time in checks per checked child is 1,004.1 ms stacked against 1,139.5 ms rebuilt, and triggers 1, 2 and 4 fired in neither build (trigger 3 not measured: fewer than 1,000 checks). 2026-11-02, restated by D10: trigger 2 reads the `Event::Checked` of a child's level (its write and diagnosis) and of the start state's diagnosis; the re-path is in `DepthStats.check_ms` and in triggers 1 and 4. 2026-11-05, trigger 3 restated by D11 stage 4 (section Bench, "The search on N forks (D11, stage 4)"): a search holds its Layers until its `close` and glibc keeps the peak, so a reading inside a search would fire on the search's own Layers; the reading is taken after each decision's `close` and compared with one session's memory. 2026-11-15, restated by D11 stage 4: the differential contract above is now `Forks` C2, a Layer diagnoses as its flat composition, since the `Stacked` port is deleted; its measurements stand. | Trigger 1: `TRIGGER_DECISION_MEDIAN_MS` = 300,000. Trigger 2: `TRIGGER_CHECK_MEDIAN_MS` = 5,000. Trigger 3, memory: `TRIGGER_RSS_KB` = `SESSION_RAM_GB` x 1024 x 1024 (7,340,032) and `TRIGGER_AFTER_CHECKS` = 1,000, against the largest RSS read after a decision's close (`DecisionRow.rss_after_close_kb`, `search_recall::rss_after_close` C1, test `trigger_3_reads_the_rss_after_close`); trigger 3, growth: `TRIGGER_CHECK_GROWTH_PERMILLE` = 1,500 against the median of the last `search_recall::GROWTH_WINDOW` = 100 checks in thousandths of the median of the first 100, read after `TRIGGER_AFTER_CHECKS` checks by `search_recall::check_growth_permille` through `trigger_lines`, test `trigger_3_reads_the_growth_limit` (the limit itself does not fire, one ms above it does, one check fewer is not measured) and `check_growth_compares_the_ends_of_the_run`; the checks of all measured decisions are flattened in row order, so on a corpus of several workspaces (one session each) the two windows can come from different sessions: the growth half is meant for the one-workspace ryo-rs run. Trigger 4: `TRIGGER_HARNESS_MS` = 600,000 against `RunInfo::search_ms` (the run less `RunInfo.outer_check_ms`, which `search-recall run` times around every outer `cargo check`), judged when `search_recall::trigger_4_judges` accepts the run's budget; contract clause C2 of `trigger_lines`, tests `trigger_4_reads_the_search_time_of_a_default_run` (the limit itself does not fire, one ms above it does, run 3's 983,033 ms with 634,965 ms of outer check does not, a narrower beam is judged, an exhaustive or unknown budget is not measured; the JSON report carries `search_ms` and `outer_check_ms`) and `trigger_4_judges_runs_within_the_default_budget` (each knob one above the default refuses, `max_depth` and `audit_every` are not compared); the run's budget and outer-check time reach `RunInfo` through `RunInfo::of_run`, test `run_info_of_a_run_carries_budget_and_outer_time`. Values pinned by `default_search_budget_is_the_decided_numbers`; `search_recall::trigger_lines` prints triggers 1, 2, 3 (memory and growth) and 4, pinned by test `triggers_follow_the_decided_numbers`. Restated 2026-11-01 (D9) and 2026-11-15 (D11 stage 4): C2 of `Forks` (a Layer diagnoses as its flat composition); tests `tests/jobs.rs a_chain_of_jobs_diagnoses_as_its_flat_composition` and `tests/forks.rs parallel_jobs_equal_serial_and_the_reference`. | The first full ryo-rs run is in this ledger: compare each trigger with its number; a trigger fires (this row is then superseded by the perf task's own record); a base larger than ryo-rs is the target. | Optimise now (parent-to-child deltas, N sessions): deltas save about 10 % of a check and add a second state to verify (the child's state on top of the parent's), a new way to be wrong. Restated 2026-11-01: its deltas half no longer holds (D9, taken up without a new measurement because CONCEPT.md decides it); its N-sessions half stands. Ignore perf and go on to M5: nobody would notice when the budget of D1 stops being met. Trigger 4 on the whole wall time, outer check included (the harness until 2026-11-01): run 3 fires it through the oracle's own `cargo check` while the search is as fast as in run 1, and no step of the work order shortens that check. Leave runs with `--outer-check` out of trigger 4: such a run still measures its search, and that measurement would be lost. Judge trigger 4 only in CI: no CI run covers the large corpus, so the trigger would never be judged where it can fire. |
| D5 unseen classes are named, not gated | A candidate names the classes of error the in-process check cannot see (`UnseenClasses`): those its marks name that the checker is blind to, and those its check observed. A blind class no mark names is in the evidence only, where the checker's whole set of blind classes is. The build after the commit settles them; nothing is built or tested before a write. Every node of the final ranking that passed the check is a candidate, in the order of the ranking. The oracle's outer check measures every candidate, or with `--outer-sample N` the top N of each decision, and its report names that sample. | CONCEPT.md Core 3 and 4: no `cargo`, `rustc` or test per candidate; what the in-process check cannot see is named in the result, and the rate at which the build after the commit rejects it is measured. Measurement: the rows of the table in section "Outer check" for the verifier now in force: small bases 1 of 125 decided candidates, ryo-rs 0 of 33 (each decision cut to 3 at the time). 33 candidates cannot tell 0.05 from 0 (the rule-of-three upper bound is about 0.09). | `unseen_classes_follow_the_marks`, `candidate_names_its_marks`, `decided_lists_every_passing_node_by_reward`, `outer_sample_takes_the_top_of_the_ranking`, `outer_sample_zero_is_refused`, `report_names_the_outer_sample` | Any row of section "Outer check" recorded after D5 shows a rejection rate above 0.05. | Keep the gate (a `cargo check` before the write, which the earlier `AfterOuterCheck` disposition stood for): it puts a build in front of every marked candidate, against CONCEPT.md Core 3. A cap in core (the 3 of the earlier `OUTER_CHECK_MAX`): it was chosen for the cost of the oracle's check, not by a stop rule of the search. |
| D6 no apply label on a candidate | A passing candidate carries no label that says it may be applied without a person or needs a review: ryo-v2 holds no registry of Rules and no disposition, and `apply` and `search` take no `--rules`. The marks and the unseen classes are named in the candidate (its `marks`, its `unseen_classes`, and its `marks_line` as the note of its `Candidate` entry in the decision log) as information; which candidate to take is the caller's. A review done before or after ryo-v2, by an AI or a person, converges on rules, and a rule is expressed ahead as a command or a mutation list. | CONCEPT.md Core 5 (the caller owns choice) and Boundary (the choice among results is outside ryo-v2). 2026-11-01. Rated against the concept: this option 8/10, A 1/10, B 2/10. The registry came from reading "review moves from the diff to the rule" as an output gate; it means that review before and after ryo-v2 converges on rules, expressed ahead as commands or mutation lists (CONCEPT.md, Use case). | `candidate_names_its_marks`, `candidate_marks_follow_its_blueprint` (core), `candidate_json_has_exactly_its_keys`, `cli_parses_apply` and `cli_parses_search` (`--rules` is refused) (host) | CONCEPT.md moves the choice among results inside ryo-v2. | A: keep the gate (a label from a registry of Rules and the marks). B: keep the types and make every candidate need a review (a dead type remains). Out of scope here: the policy of the input (CONCEPT.md, the form of the input). |
| D7 blind classes are declared, an unjudgeable file is a failure | The checker declares the classes of error it does not see (`Verifier::blind_classes`, with no default: an empty set claims it sees every class), and the evidence of a decision lists them (`Evidence.blind_classes`, `evidence.blind_classes` in the host's JSON). A candidate names a class when its marks name it and the checker is blind to it, or when its check observed it on a file of the check (`Verdict.unseen`, from `ScopedFindings.unseen_at`, the one record of each class's files; today `UnexpandedMacro`, the unresolved-path detector silent on a file with a macro call that did not expand). A mark says what an edit can do and a blind class what the checker does not see; a candidate names a class only when both hold. The rust-analyzer adapter declares `Borrow`, `BuildScript`, `ProcMacroCrate`, `Visibility`, `ConstEval`, `StructLiteralPath` and `UnusedTypeParam` always, `DeriveWithoutServer` with the proc-macro server off, `UnresolvedPath`, `DuplicateName` and `FieldBounds` with the gap detectors off, and `TestCode` with `cfg_test` off (on by default). A file the check cannot judge is a failure with its reason: a manifest (`Blocking::Unverifiable`) and a file on which a gap detector panicked in the check but not at baseline (`CheckError::DetectorPanicked`, `detector_panicked` in the JSON; a session error wins over it). A panic the baseline had on the same file is a blind spot of that detector there before any edit: the check does not fail, and the detector's class (`GapDetector::unseen_class`) is named as observed; otherwise one such file would fail every candidate whose scope holds it. A build script and a proc-macro crate are read, and what they do is not seen: they are unseen classes, not failures (CONCEPT.md Core 3). | CONCEPT.md Core 3. 2026-11-01. A gap of the checker (CHECK_LEDGER G-B-2, G-B-5, G-B-8, the server off, a switch off) was in no result, and a candidate whose file a detector had panicked on passed with that detector's findings gone. Rated against the concept: this option 8/10, A 2/10, B 5/10, D 2/10, the audit's proposal 3/10. | `unseen_class_names_are_stable`, `unseen_classes_from_an_empty_set_is_none`, `from_baseline_carries_the_observed_unseen`, `candidate_names_what_it_touches_and_what_was_observed`, `empty_blind_names_nothing_from_marks`, `evidence_carries_the_checkers_blind_classes`, `check_error_reach_is_decided_per_variant` (core), `ra_blind_classes_follow_the_config`, `unexpanded_macro_file_names_the_class`, `session_errors_win_over_a_detector_panic`, `ra_config_default_diagnoses_test_code` (ra), `decided_lists_the_checkers_blind_classes`, `decided_evidence_has_exactly_its_keys`, `check_failed_names_the_panicked_detector` (host). Restated 2026-11-02 (D10): C1 of `ScopedFindings::unseen`, C3 of `ScopedFindings::on_top_of`, C10 of `RaAnalyzer::check` and C18 of `Forks` (2026-11-15, D11 stage 4, in place of C4 of the deleted `Stacked` adapter); tests `unseen_is_the_classes_of_unseen_at`, `on_top_of_drops_unseen_of_files_diagnosed_again`, `a_diagnosis_names_where_it_observed_each_unseen_class`. | The checker sees a class it declared blind: take the class out of its declaration, and out of the candidates that named it. | A: leave it as it was (the blind classes in no result, a detector panic passing). B: name every blind class on every candidate (a mark that no edit touches would be named, and the name would say nothing). D: a candidate with a blind class is a failure (the checker is always blind to some class, so no candidate would pass, against CONCEPT.md Core 3). The audit's proposal to fail an edit of a proc-macro crate: it does not fit the CONCEPT.md sentence that a file read with its effect unseen is an unseen class. |
| D8 a write is checked again on the current text | Before a candidate's edits are written, `prepare_write` (`ryo-v2-core::app::write`) takes the chosen candidate of a `Decided` resolution and checks, in this order: its basis has no drift against the current text (`WriteRefusal::Drift`); no edit creates a path that the basis lacks and that exists now (`Appeared`); the session has read the current text, that is each file of the session's provenance has the fingerprint of its current text (`StaleSession`; a provenance that lists no file cannot be compared and is refused, `UnrecordedSession`). A pick that no candidate or more than one candidate ends in is refused (`NotACandidate`, `AmbiguousPick`), so the edits are never another candidate's. It then checks the edits again with `Verifier::check` on the current text at the scope the decision was made at, against a baseline taken once for the call (`NewErrors`, or `Check` when the check fails). A refusal before the re-check calls no check. What it returns is a `CheckedWrite`: the edits, the verdict of the re-check, and the unseen classes named at the checker's blind classes when it is written together with those the re-check observed. Only `prepare_write` builds one, and it writes nothing to disk. The verdict of a decision, of a search and of this re-check is one function, `app::verdict_of`. `Candidate` C1 [invariant] is now a statement about the value (its edits are the texts of the state its basis was taken on, and its basis is what `prepare_write` refuses a write on); the sentence that nothing splices the edits onto a changed text is gone from it. | CONCEPT.md Boundary (what it writes: the picked result, when its basis has no drift, checked again) and the Writer of the execution model. 2026-11-01. A basis is the files a verdict was taken on, not the set of files the check read, so a file of the scope outside it can change without any drift (audit D4, c5), and nothing re-checked the edits before a write. Cost: the re-check is one `Verifier::check` at the decision's scope; the session must have read the current text, so after any change of a file the provenance holds (an unrelated file included) the session is reloaded before a write, about 40 s on a large base (D2), and that file alone gives `StaleSession`. Coverage: the provenance holds the indexed files, the manifests and the lock file, compared with the fingerprint of their bytes on disk. It does not hold the toolchain or an input that is not indexed (`.cargo/config.toml`, for one), and a path that is in neither the basis nor the edits and that was made after the load is not seen either (the current text is a lookup by path and cannot list the files). Rated against the concept: this option 8/10, A 3/10, B 4/10, D 5/10. | `clean_recheck_returns_the_edits`, `drifted_candidate_is_refused_and_never_checked`, `edit_creating_a_file_that_now_exists_is_refused`, `stale_session_is_refused_and_never_checked`, `new_error_on_the_current_text_refuses_the_write`, `recheck_names_the_unseen_it_observed`, `drift_wins_over_a_stale_session`, `recheck_uses_the_decided_scope`, `unknown_pick_is_refused`, `ambiguous_pick_is_refused`, `decide_and_write_paths_give_the_same_verdict`, `candidate_names_what_it_touches_and_what_was_observed` (core), `provenance_fingerprints_are_of_the_disk_text` (ra); contract clauses C1 of `CheckedWrite` and of `Candidate`, C1 to C9 of `prepare_write`, C1 of `verdict_of` and of `named_unseen`. | The verdict is known to stay valid while the set of files a check reads is unchanged (a verdict cache is measured), so the re-check can be skipped in that case; or a change of a file outside the basis and the provenance is found to reach a written result. | A: reword the promise only (3/10): no code keeps it, and the drift of a file outside the basis stays unseen. B: add the host's write command now (4/10): it needs the update of a session to the current text, which belongs to the session's model and is not decided here. D: widen the basis to everything the check read (5/10): the re-check becomes unnecessary, but the basis grows with the scope, a change of one file outside the change decides it again, and it comes close to the design of a verdict cache. |
| D9 stacked writes | A fork of the analysed workspace takes writes stacked on top of earlier ones as jobs of the port `Forks: Verifier` (`ryo-v2-core::app::ports`; D11 stage 3b): `open` freezes the base and gives its root as a Layer; `submit` queues a job that writes its edits on top of a finished Layer, resolved against that Layer's state (C13), diagnoses its scope there and finishes as a new Layer; `next` gives the outcomes; `layer_files` gives the files a check covers for the edits that take the base to a Layer's state, derived from the jobs of its chain; `layer_text` reads a Layer; a Layer is dropped, never reverted; `close` withdraws the jobs and thaws the base. A job is diagnosed under the rules of a check: D1's cut-off (C11, C17); D2's sampled isolation of the base, counted per write, and the revert audit of its paths; D7's detector-panic rule, judged against the baseline whatever its parents had (C19); the unseen classes its diagnosis observed (C18). A Layer diagnoses as its flat composition, per file, unseen classes included, and every class a diagnosis names has its files: the result is that of a check of the edits that take the base to the Layer's state (C2). `Verifier::check` is the one-job case (C12); a session error of its release wins over every other error of a check; a check with no edit diagnoses no file. Nothing but the flat reference check writes the base (D2): a write the frozen base refused poisons the session and withdraws every job not yet delivered (C7), a panic inside a job's layer poisons that layer only (`impl Forks` C8), and on a poisoned session nothing more is written to the database; the reads of `Verifier` describe the base (C21). The rust-analyzer adapter keeps every fork of a session as layers of the Engine (D11): one live fork per session, `Analyzer` forks included; while forks are open `check`, `audit_revert`, `Expander::expand` and another `open` are `ForkInUse` (C6), a Layer of another session is `ForkInUse` before anything else (C5), and an expansion of a Layer is `Forks::expand` (C10, D10). `open`, `submit`, `expand`, `next`, `cancel` and `close` take the session mutably. `Analyzer` keeps its API and its one-write rule on one layer, and its discard writes nothing back. | CONCEPT.md, Execution model: "Writes stack: a child is written on top of its parent, never rebuilt from the base ... This is the first condition of the model"; "Forks: N live (4-16 *), writes stack on the parent, nested forks". 2026-11-01. A verifier-side port keeps one implementation of D1, D2 and D7 for checks and levels; the decision use cases (`apply_blueprints`, `search_decision`, `prepare_write`) depend on `Verifier` alone, through `app::verdict_of`, and `Analyzer` has no place for the detector-panic rule. Judged against the baseline, a detector panic keeps the stack equal to its flat check; against the level below it would not: a panic brought back above a level that removed it would fail, and one on a failed level built upon would pass. Rated against the concept: this option (A2) 8/10, A 4/10, D' 5/10, B 3/10. Cost: the one-level case does what a check did; a sampled level above the first adds one diagnosis of its scope before its write; the gain of stacking over rebuilding for the search is measured on the small bases and on ryo-rs in section Bench, "Stacked against rebuilt" (D10); on ryo-rs (2026-11-03) the time in checks per checked child is 1,004.1 ms stacked against 1,139.5 ms rebuilt, with 1.14 level writes per checked child. 2026-11-15, restated by D11 stage 4: once the search ran on `Forks` the serial stack of levels (`Stacked`, its tickets, `pop`, `release`, `levels`, `fork_text`, `files_of`) had no caller and was deleted with the adapter's `RaStack` and `tests/stack.rs`; stacked writes are chains of jobs, each a Layer on its parent's, so a level is dropped, never reverted, and the undo log is deleted with it. The stacked-against-rebuilt measurements above stand as the record of the serial stack; the stage-4 gates (section Bench, "The search on N forks (D11, stage 4)") measure the chains of jobs. | The `# Contract` clauses of `Forks` (C1-C8, C10-C21) and `Forks::layer_files` (C1, C2) (core); `impl Forks for RaAnalyzer` (C1-C11), `Verifier for RaAnalyzer::check` C1, C2, C6, C7, C8, `Analyzer for RaAnalyzer` (C1-C7), `RaConfig.assert_revert_every` C1, `with_check_timeout` C1 (ra); the tests of `tests/jobs.rs`, `tests/check.rs` and `tests/forks.rs`, in particular `a_chain_of_jobs_diagnoses_as_its_flat_composition`, `check_is_a_one_job_session`, `a_job_writes_against_its_parents_state`, `files_outside_a_jobs_scope_keep_its_parents_findings`, `a_new_detector_panic_fails_the_job_and_keeps_its_parent`, `a_baseline_panic_brought_back_by_a_job_is_named`, `layer_files_and_of_level_equal_the_check_over_a_chain`, `a_diagnosis_names_where_it_observed_each_unseen_class`, `a_finding_is_on_a_file_of_its_diagnosis`, `a_panic_in_a_layer_poisons_only_that_layer` (jobs), `text_is_the_base_after_a_poisoned_check`, `a_session_error_of_the_release_wins_in_a_check`, `session_operations_refuse_while_an_analyzer_fork_is_live` (check), `session_operations_refuse_while_forks_are_open`, `a_layer_of_another_session_is_refused`, `parallel_jobs_equal_serial_and_the_reference` (forks); and the unchanged `tests/verifier.rs`, `tests/timeout.rs`, `tests/smoke.rs` and the oracle tests (ra). Restated 2026-11-02 (D10) and 2026-11-15 (D11 stage 4): the clauses of `Stacked`, its methods and its adapter went with the port; each with a `Forks` counterpart is now that clause (`write` C2 and C3 are C13 and C2; `diagnose` C1, C3, C4, C5, C6, C8 and C9 are C14, C15, C17, C18, C19, C18 and C20; `files_of` C1 and C2 are `layer_files` C1 and C2; `Stacked` C2 is C5 and C6, C3 is C21, C4 is C12); `-levels`, `-fork_text`, `-pop`, `-release`, `diagnose` C7 and the tickets have none, and the level-count, ticket and event-shape assertions of their tests went with them. | A caller needs a shape `Forks` cannot give, such as a job on a Layer still in flight or a write that does not take the session mutably (the copy-on-write forks the serial port could not carry are `Forks`, 2026-11-15); a search on N forks finds a divergence from the flat reference check the differential tests (`parallel_jobs_equal_serial_and_the_reference`, `search_verdicts_equal_the_reference_check`) do not cover; upstream salsa gains a writable fork. | A: grow `Analyzer` (apply pushes a level, `AlreadyApplied` removed): it bypasses D1, D2's sampling and audit record, and D7's detector-panic rule, which live in `Verifier::check`. D': a fork type that borrows the session mutably for its life: exact exclusion by type, but forks with several live at once cannot each borrow their session. B: a third port beside `Verifier` and `Analyzer`: the rules of a check copied. Judging a level's detector panic against the level below: the stack would not diagnose as its flat composition. Refusing a write of a path a lower level deleted: the stack would refuse what the flat check accepts. Restoring the remaining levels after a revert failure (every revert failure poisons the session): it writes to a database the failure declared unreliable. The level count kept in the ticket: a second record of the levels that can disagree with the session's. Today's error order in a check (a Blueprint error over a revert mismatch): the caller gets a Blueprint reach for a poisoned session. |
| D10 the search writes children on top of their parents | `search_decision` opens the session's forks (D9, D11) once, before its first job, and closes them before its revert audit. Each verified node owns a Layer: the root the Layer of the start state's job (the base's root for an empty start), a passing child the Layer of its own check job. A child is the job `Forks::submit` of its own edits on its parent's Layer, at the decision's scope, with `Limit::Check`; no job writes a node's edits from the base, and none writes a parent's path again (C12). A parent is expanded by a read job, `Forks::expand` of its Layer with the fingerprint of its state's edits, which writes nothing (`Forks` C10). The order of the search is unchanged: outcomes are consumed in submission order, and tree insertion and the duplicate check happen at submit (C16, D3). Every verdict is formed by one function, `Verdict::of_level`: a job's diagnosis on top of its parent's findings (`ScopedFindings::on_top_of`, with the paths the child deletes, `Edit::deleted_paths`), kept to the files a check of the edits that take the base to the job's state covers (`ScopedFindings::within`; `Forks::layer_files`, which derives those paths from the jobs of the Layer's chain, and gives every file of the state at `Scope::Workspace`), against the baseline (C13). A check is its one-level case, `Verdict::of_check`: the verdict of `app::verdict_of`, used by `apply_blueprints` and `prepare_write`, and of `app::verify`, the bench's rebuilt measurement path; `bench --on` forms its stacked verdict with `Verdict::of_level`. It holds for any `Forks` that keeps the port's clauses C2, C14, C15, C18 and C20 and `layer_files` C1, while a step only writes (`stage` C3); `compose` drops a delete of a path the base lacks (`compose` C1). An unseen class is recorded with the files it was observed on (`ScopedFindings.unseen_at`), and the set of classes derives from that one record. Every finding of a diagnosis and of a check is on a file of it (`Forks` C20, `Verifier::check` C1). The start state is one job, with `Limit::None`, consumed before depth 1. A failed call of the forks, or a start job's failure of reach Session, is `SearchError::Fork`; a child's failure of reach Session is `SearchError::Check`; a `layer_files` of reach Blueprint is the check failure of its child (C14). A failed close wins over a report and over `NoCandidates` (C15). A failed or withdrawn child's Layer is dropped when it is consumed, a parent's once its depth is consumed, and every Layer before `close` (C18). | CONCEPT.md, Execution model: "Writes stack: a child is written on top of its parent, never rebuilt from the base ... This is the first condition of the model"; the Expander is "read-only". 2026-11-02. The verdict equals a check of the composed edits in files, findings and unseen classes: a file outside a level's scope has its findings at the level below (D9), the check's files are those `files_of` gives, and the baseline difference pairs per file. Results are equal for a decision that hits no wall-clock budget, no refusal of a level outside a child and no failure of a sampled write's below-level diagnosis. Rated against the concept: re-path in today's order (O1) 8/10, depth-first order 3/10, re-ordered parents 4/10; the expander on the fork (X2) 8/10, its own fork 6/10, the ticket passed to it 5/10; one verdict function with the check as its one-level case (V3) 9/10, a second composition kept to every diagnosed file 6/10, the union of the levels' classes 4/10, a Workspace scope per child 2/10. Measured on the small bases and on ryo-rs: section Bench, "Stacked against rebuilt". "Rebuilt" is the commit that added the measurement fields (dda64c4), built in a second worktree, and "stacked" is the commit of the switch (eff6820). Both were run back to back on one machine, and their shas and the run order are named there. 2026-11-02, small bases (12 threads, 30 GiB, otherwise idle): every non-time field of the 45 rows equal in both builds, no decision with a wall-clock effect; time in checks per checked child 10.2 ms stacked against 10.4 ms rebuilt, 1.22 level writes per checked child. 2026-11-03, ryo-rs (the decision corpus of section "Large runs", step 1, regenerated with main 531fc18: 11 decisions, 2 of them pairs; 12 threads, 30.27 GiB, otherwise idle; release builds; "stacked" is main 531fc18, which contains eff6820): every non-time field of the 11 rows equal in both builds, no decision with a wall-clock effect; time in checks per checked child 1,004.1 ms stacked against 1,139.5 ms rebuilt, 1.14 level writes per checked child. The revisit item "the time in checks per child stacked exceeds that rebuilt on the ryo-rs corpus" does not fire. A level's file list is files x one entry, for at most `max_depth` + 1 levels (derived from `ryo-v2-ra` `fork.rs`); its memory is the RSS rows there. 2026-11-15, restated by D11 stage 4 (section Bench, "The search on N forks (D11, stage 4)"; 12 logical cores, 6 physical × 2 SMT, 30.27 GiB): the re-path of one stacked fork (`repath`, deleted with it) is replaced by a Layer per verified node, so no job writes a parent's path again. W4-search met: every row with no wall-clock effect equal in every non-time field to the serial build of 3fdf0fa, which re-pathed (small bases 45 of 45, ryo-rs 11 of 11 at W = 1), and the oracle's differential test is `search_verdicts_equal_the_reference_check`, at W = 1 and 4 against the flat reference check; time in checks per checked child on ryo-rs 660.4 ms at W = 1 against 638.2 ms on the serial Engine and 1,044.7 ms on the undo log, so the revisit item on the time in checks does not fire. The memory of a node's Layer is D11's (`search_layers`, `max_forks`). | C1, C7, C12-C16, C18 of `search_decision`; C1-C3 of `Verdict::of_level`; C1 of `Verdict::of_check`; C1 of `Edit::deleted_paths`; C1 of `app::verdict_of`; C1 of `app::verify::verify`; C3 of `stage`; C1 of `compose`; C1 of `DecisionRow` and C1 of `Report` (oracle); C1-C4 of `ScopedFindings::on_top_of`; C1, C2 of `ScopedFindings::within`; C1 of `ScopedFindings::unseen`; C2, C5, C6, C10, C13-C15, C18, C20 of `Forks`; C1, C2 of `Forks::layer_files`; C1 of `Verifier::check`; C7, C10 of `impl Forks for RaAnalyzer`; C10 of `RaAnalyzer::check`; C18 of `impl Expander for RaAnalyzer`; C2 of `RaAnalyzer::reference_check`; the tests they name, in particular `search_verdicts_equal_the_reference_check` (oracle; every checked node of the ten depth-2 cases of search-lab against the flat reference check, at W = 1 and `SEARCH_FORKS`), `the_reference_check_is_a_fresh_load` (oracle; the anchor of that reference), `search_and_apply_give_the_same_verdict_on_a_chain`, `layer_files_and_of_level_equal_the_check_over_a_chain`, `a_diagnosis_names_where_it_observed_each_unseen_class`, `a_finding_is_on_a_file_of_its_diagnosis`, `a_failed_base_read_is_a_session_error`, `an_expansion_of_a_layer_is_that_of_its_state` and `an_expansion_sees_a_file_the_state_created` (ra), `children_are_jobs_on_their_parents_layer`, `parents_are_expanded_on_their_layer`, `the_verdict_keeps_the_findings_below`, `the_start_state_is_one_job`, `parallel_search_equals_serial`, `layers_follow_the_retention_rule`, `decide_and_write_paths_give_the_same_verdict`, `verify_forms_its_verdict_as_a_check_does`, `deleted_paths_are_those_with_no_text` and `own_edits_only_write` (core), `row_json_keeps_its_keys_and_values` and `report_json_keeps_its_keys_and_values` (oracle). Restated 2026-11-15 (D11 stage 4): C1 of `-repath`, the clauses of `Stacked` and its adapter, `Expander::expand` C1 and C16, C17 of the adapter's `Expander` (an expansion on the top level of a live stacked fork) went with the re-path and the port; their `Forks` counterparts are above. | A command that deletes a file is added (`stage` C3 no longer holds: the verdicts then rest on `compose` C1 dropping a delete of a path the base lacks and on each job passing the paths it deletes, or they part); test `search_verdicts_equal_the_reference_check` fails, or the comparison of the two builds finds a differing row among the decisions with no wall-clock effect (no `budget_exhausted`, no `soft_stopped`, no `TimedOut`); the time in checks per child stacked exceeds that rebuilt on the ryo-rs corpus; the RSS rows show memory growing per level, or a depth limit above D1's makes more levels live (then keep each level's delta of files only); the search order changes (as a Monte Carlo search, not yet built, would), which changes which Layers a search holds (`search_layers`, D11). | Depth-first order: the prune and the judge call would change, so the results would. Parents re-ordered to share prefixes: the check order decides duplicates and the budget cut. The expander on its own `Analyzer` fork: each parent rebuilt from the base for its expansion. The ticket passed to the expander: ties the `Expander` port to the stacked fork's ticket (the port deleted 2026-11-15; the expansion of a Layer is `Forks::expand`). A second composition of the verdict in the search, kept to every file a level diagnosed: two ways to form a verdict, and more unseen classes than the check names. The union of every level's unseen classes: names a class a later level no longer has. A second `pub` field for the files of the unseen classes: two records that can disagree. A Workspace diagnosis per child: against CONCEPT.md Core 4. |
| D11 the in-memory fork Engine | Forks with several live at once are layers of one frozen analysed database in one process: a layer is one write on top of its parent (the base or a sealed layer); a fork at level k is the chain of its layers; reads resolve through the chain, reaching each reader through the handle; writes go to the top layer; nothing writes the base or a sealed layer except the listed shared writes; a query on a sealed layer runs in a throwaway child. Validity is salsa's own revision test on that chain (linear-history equivalence: a fork reads what a database that applied the chain's writes one after another would read; the invariants E1-E9 that keep this are marked at the points of the vendored patch they govern). Tracked structs keep their ids under a layer. The mechanism is a patch of salsa 0.28.5 and salsa-macro-rules 0.28.5, vendored outside the workspace members and wired by `[patch.crates-io]`, carried here and shaped so it could go upstream; a handle without a layer behaves as upstream, checked by salsa's own test suite (`just salsa-upstream`, in `pre-push` and on every change to `vendor/`); RA is unchanged. Sealing is a type (`OpenLayer` / `SealedLayer`) and the base brand is structural. `ryo-v2-engine` (its own crate, `publish = false`) binds it to RA's database; `ryo-v2-ra` keeps the rules of a check and an independent flat reference check. Neither the Engine nor anything that depends on it is published in this phase. Built in four stages, each with go/no-go gates fixed before its run: (1) measurement probe, (2) overlay prototype over body-level and item-level edits, (3) the Engine behind `Stacked` (deleted at stage 4) and a new `Forks` port, (4) the search running N forks. Stage 1 is built (the probe `engine-probe`, `RaSession::memory_report`, `Event::Indexed`, the one stacked procedure `stacked::stacked_series` that the probe and the bench share, on `Forks` at W = 1 since stage 4, the `/proc` reads that fail instead of giving no value, the `audit_revert` fix) and changes nothing in the search. The Engine's reads under a deep chain (the chain views, 2026-11-04): a sealed layer's memos, tombstones and tracked-struct entries are read from one view of its chain; a memo of durability above `LOW` is checked against the base's last-changed revision before any stamp; a layer's `LOW` last-changed revision and an ancestor's cancellation are read without a walk; a `LOW` memo's stamp is still looked up layer by layer, newest first, and not flattened. N live forks in parallel (stage 3b, 2026-11-05): they run in-process on the Engine's job pool (`ryo-v2-engine` `Jobs`, one per open forks session, joined at `close`) behind the `Forks` port of `ryo-v2-core`, on synchronous worker threads, not async. The usual shape for such work is a dedicated thread wrapped in async with cancel; the synchronous pool is chosen instead, by this decision and its benchmark (where parallelism is the domain, the concurrency model is chosen by a recorded decision and a benchmark), and keeps that shape's substance: dedicated workers own the job state, the caller reaches them only through `submit`, `next(timeout)` and `cancel`, every job can be withdrawn, and the port's handles need no `Sync`, so an async front can wrap `next` later. The budget, fixed per pool: `W = min(N, T_w)` jobs at once, `T_w = max(1, T − c)`, `t = max(1, T_w / W)` threads for one job's diagnosis, where `c = 1` when the caller works between calls (stage 4's shape, `ForksConfig.caller_works`, the default) and 0 when it only waits in `next` (the bench). One job writes a child of a finished layer (or of the base's root), diagnoses it and seals it; its edits are resolved at `submit` on the session thread. Whether a job was withdrawn is decided only from the job's atomic state (a compare-and-swap), never from the kind of its error. A salsa cancellation is `Cancelled` wherever a job raises it, a gap detector included; `PropagatedPanic` stays `Panicked`. A load with `RUST_ANALYZER_USE_POSTCARD` set is refused (`AnalyzerError::Load`; a clause of `RaConfig.proc_macro_processes` since stage 4): that protocol's callback runs queries under the proc-macro process lock, a cycle across layers salsa cannot see. The search on N forks (stage 4, 2026-11-05 to 2026-11-15): `search_decision` runs on the `Forks` port (D10), each verified node owning its Layer, with at most `Forks::width` jobs submitted and not yet received; the host and the oracle ask `SEARCH_FORKS` = 4 with a caller that only waits (c = 0), and the search waits in `next` for `SEARCH_TICK_MS` = 100 ms before it reads its stops again. Outcomes are consumed in submission order, so no result depends on W or on the order the jobs finish in (C16). The RSS stop is relative to the RSS at the search's start (`MemoryBudget`, `RssStop`, C17); the Layers a search holds follow a retention rule (C18) and are bounded by `search_layers` (73 at 20 roots and W = 4), within `max_forks(30, 4)` = 294 (Table M4). Every fork of a session (a job, an `Analyzer` fork, a check, the one-job case) is layers of the Engine over the frozen base: the undo-log backend is deleted (`RaConfig.engine`, `Backend`, the `--engine` flags of `bench`, `engine-probe` and `search-recall run`, and the reports' `backend` key; `width` is the first key of the search-recall report), and the `Stacked` port is deleted with the adapter's `RaStack`. Nothing writes the base but the flat reference check (`RaAnalyzer::reference_check`, `flat.rs`, R1), which runs no line of the layer code, is tied to a fresh load of the state, and re-asserts its level; the differential tests compare the Engine's checks, jobs and search verdicts with it. | CONCEPT.md, Execution model ("Forks: N live (4-16 \*), writes stack on the parent, nested forks"; "N forks then share the cores"); the maintainer's decisions of 2026-11-03: the forks are in memory, in one process; the Engine is a separate crate; the salsa patch is carried in this repository; neither the Engine nor anything that depends on it is published; the operating system's fork is rejected (reasons in the rejected-options column). Rated against the concept: revisioned layers (d+) 8/10, overlay validated by changed inputs (d) 6/10, copy-on-write memo tables (a) 4/10, N databases (b2) 2/10, an own engine (c) 1/10; the patch carried as vendored salsa and salsa-macro-rules 8/10, a fork crate in the workspace 3/10. Measurement, stage 1, 2026-11-03 (section Bench, "Memory composition (engine probe, D11)"; 12 threads, 30.27 GiB; synchronous drops): the ryo-rs base holds 4.908 GiB RSS and 4.016 GiB live heap after its baseline; one check keeps 0.049 to 0.338 GiB of heap after its write. G1 not refused (N_low = 52, at least 50 under the stricter reading; a lower bound, M2 of stage 2 decides the memory); G2 not met (salsa's report gives shallow sizes only: 0.284 GiB of the 4.016 GiB live heap); G3 met (tables S1 and S2 filled for ryo-rs); G4 not measured. The `LOW` stamp walk kept, 2026-11-04 (12 threads, 30.27 GiB; ryo-rs, `bench <BASE> --depth-sweep 1,5,10,20 --depth-files 4 --repeat 3 --scope affected --engine`, release, without `fork-probe`, two runs, warm min/median/max in ms): depth 1 43.2/43.2/43.8 and 42.8/42.8/43.4, depth 20 44.6/44.6/45.7 and 43.1/43.1/45.2, so 1.03 and 1.01 times depth 1 (gate 1.10; 1.25 to 1.29 before the durability shortcut, the `last_written` without a walk and the cancel epoch). Stamp visits per warm check of the top level (`fork-probe`, the counts of `--repeat 3` less `--repeat 1`, halved): 131.1k at depth 1, 79.7k at depth 20, 0.61 times (ceiling 1.2). After the shortcut a warm `LOW` stamp lookup hits in the reading layer and a fresh child's first in its parent. A stamp view would add about 56k + 9k·d keys, about doubling the chain views' bytes (6.45 MB, 4.02 % of a depth-20 stack's 160.3 MB heap, `engine-probe --depth 20`). Stage 3b, 2026-11-05 (section Bench, "Forks in parallel (D11, stage 3b)"; 12 logical cores, 6 physical × 2 SMT, 30.27 GiB, release): the pool rated an own fixed pool per forks session 8/10 (an explicit budget, joined with the base's lifetime, no global state), a file-task pool across jobs 6/10, rayon's global pool 3/10. On ryo-rs, a tree of a seed and 4 levels of 24 jobs, two runs: small scope (2 files of `ryo-ui`) 3.02 jobs/s at W = 1, 10.55 at W = 4 (3.49 times; gate X3b 1.5 times), 17.10 at W = 12 (5.65 times); medium scope (44 to 47 files) 1.19, 2.99 (2.51 times), 3.93 (3.30 times); W4 0 differences in every run; the serial path gains no overhead from the pool (T1-3b); Core 4 under contention holds at W = 12 (median 436 ms, max 3,552 ms, none cut off); the RSS never reached its stop (peak 5.74 GB). Stage 4, 2026-11-05 (section Bench, "The search on N forks (D11, stage 4)", Tables S4 and F3; 12 logical cores, 6 physical × 2 SMT, 30.27 GiB, release, against the serial build of 3fdf0fa): I2, W4-search, C4-4 and T1-search met (660.4 ms of checks per checked child at W = 1 against 638.2 on the serial Engine and 1,044.7 on the undo log; recall 0.911 and 0.900 at W = 1 and 4); X4 missed on ryo-rs (the median decision at W = 4 is 0.80 times W = 1, target 0.67), met on the small bases (0.61); M-4 met in part; `SEARCH_FORKS` = 4 from Table F1 (a check at W = 4 costs about what it costs alone; the median doubles at 8). Stage 4, 2026-11-15, the undo log deleted (1a9db79): `search-recall run corpus/search` gives the same 45 rows as the build of c51048f, 0 differing, recall 0.911, verdict adopt (a comparison of the rows' fields; no time figure is recorded from it). Two deviations from the plan of stage 4: the report carries no `build` hash key, and the `--engine` flags were removed (`search-recall run --engine` is an unknown argument) rather than kept as accepted no-ops. | Stage 1, in force: `RaSession::memory_report` C1-C3 (`tests/memory.rs memory_report_lists_ingredients_and_memos`, `memory_report_changes_nothing`, `memory_report_refuses_while_a_fork_is_live`); `Event::Indexed` C1 (`tests/smoke.rs indexed_comes_before_loaded`); `ProbeReport` C1-C3 (`engine_probe::tests::report_json_keeps_its_keys_and_values`, `delta_and_freed_read_the_allocator`, `a_failed_proc_read_is_an_error`) and the counting allocator's `counting_allocator_follows_allocations`; `stacked::stacked_series` C1, C2 (`probe_and_bench_run_the_same_order`, `a_stacked_verdict_is_formed_as_the_search_forms_it`, `a_failed_hook_read_ends_the_series`, `a_failed_close_keeps_the_series_error`, `a_hook_error_names_the_failure_it_cuts_short`; on `Forks` since stage 4, with `a_poisoned_close_is_an_isolation_failure`), so the probe measures the procedure the bench times; the bench occupancy C1 (`bench::tests::occupancy_is_cpu_over_wall_times_threads`); the `/proc` reads of `proc.rs` (C1 of `read`, C1 and C2 of `parse_kb`, C1 of `vm_rss_kb`, `mem_total_kb` and `cpu_ms`, with the `proc::tests` they name); C3 of `RevertAudit for RaAnalyzer` (`tests/timeout.rs a_failed_audit_keeps_its_paths`); the unsafe ceiling `crates/ryo-v2-core/tests/unsafe_ceiling.rs` (`unsafe_code = "deny"` for the workspace, `vendor/` excluded, the probe's `alloc.rs` the one allowance and the one row of `UNSAFE.tsv`). Pending, not built: stage 2's gates V2, I2, P2, T1 and M2 and salsa's upstream suite (`just salsa-upstream`); stage 3's gate V3; stage 4's gate W4 (run 2026-11-05 as W4-search, Table F3); the tests those gates name (the shared-write list, sealed layers, E6 out of queries, isolation, the differential tests against the reference checks, on both backends until the undo log was deleted 2026-11-15; since then against the flat reference check). The `LOW` stamp walk: the ceiling `vendor/salsa/tests/fork_probe.rs a_low_memo_stamp_lookup_is_bounded` (on 20 diagnosed layers, 1 layer visited by a warm check in the top, at most 2 by a fresh child's first check, none for a `HIGH` memo), with `a_durable_memo_under_a_deep_chain_visits_no_stamp`; the base path stays upstream's (`tests/fork.rs a_base_handle_shallow_verify_is_upstreams`, `just salsa-upstream`). Stage 3b: the clauses C1-C8 of the `Forks` port (`ryo-v2-core` `app::ports`) and the tests they name (C9, the Engine backend a `Forks` session needed, went with the undo log at stage 4); in `ryo-v2-ra` `tests/forks.rs`, `parallel_jobs_equal_serial_and_the_reference` (W4: an 85-job tree, 20 seeds, W = 4 and 8 against W = 1, 84 jobs against a twin session's flat check), `a_worker_reused_across_layers_keeps_e6` (E6 on a reused worker), `isolation_holds_after_parallel_jobs`, `job_threads_times_jobs_fit_the_budget` and `a_cancellation_is_cancelled_wherever_a_job_raises_it`; in `ryo-v2-engine` `tests/jobs.rs`, `cancel_true_is_always_withdrawn` (the cancel race) and `width_and_job_threads_fit_the_budget`; `tests/timeout.rs a_cancel_inside_a_detector_is_cancelled`; `session::tests::postcard_protocol_is_refused_at_load`, the test of C1 of `RaConfig.proc_macro_processes` since stage 4. Stage 4: C1-C18 of `search_decision` and the tests they name, in particular `parallel_search_equals_serial` (a fake finishing jobs in random order, W 1 to 8, 20 seeds, the whole report equal to W = 1), `the_window_never_holds_more_than_w`, `the_rss_stop_is_relative_to_the_search_start`, `layers_follow_the_retention_rule` and `children_are_jobs_on_their_parents_layer`; `SEARCH_FORKS` and `SEARCH_TICK_MS` pinned by `default_search_budget_is_the_decided_numbers`; C1 of `max_forks` (`app::memory::tests::max_forks_follows_table_m4`), of `search_layers` (`the_searchs_layers_fit_max_forks`) and of `MemoryBudget`, C1 and C2 of `RssStop`; C10-C21 of `Forks` and C8-C11 of `impl Forks for RaAnalyzer` with the tests of `tests/jobs.rs` they name; C1-C3 of `RaAnalyzer::reference_check` (`tests/check.rs reference_check_runs_no_layer_code`, `the_flat_reference_reasserts_its_level`, oracle `tests/search_recall.rs the_reference_check_is_a_fresh_load`); oracle `search_verdicts_equal_the_reference_check` (W = 1 and `SEARCH_FORKS`); the report's keys, `width` first and no `-backend`, by `search_recall::tests::report_json_keeps_its_keys_and_values`; `--engine` refused by `run_args_refuse_what_they_refused_before` (search-recall). | A stage gate fails (G1, V2, I2, P2, T1, M2, V3, W4); salsa is bumped by an ra_ap bump (the patch is re-applied and every Engine test re-run as one change); upstream salsa offers a fork or overlay API; a stacked search on the Engine finds a divergence from the reference check; publishing becomes a goal. The `LOW` stamp walk: `a_low_memo_stamp_lookup_is_bounded` fails, the stamp visits per warm check at depth 20 exceed 1.2 times those at depth 1, or the depth gate (the warm median at depth 20 at most 1.10 times depth 1, in two runs) fails; then a stamp view is built (the last stage of the chain views). Restated 2026-11-05 (stage 4): `bench --depth-sweep` now runs on `Forks`, and its warm figure is a whole job on a new Layer at depth K (write and diagnosis), not a cached re-diagnosis of the stacked fork, so the 1.10 above was measured on another procedure. On the job figure, depth 20 is 1.097 and 1.092 times depth 1 (bench's lower middle; 1.111 and 1.100 by the mean of the middle two) on ryo-rs, 12 cores, 30.27 GiB. The stamp view is now revisited when `a_low_memo_stamp_lookup_is_bounded` fails, when the stamp visits per warm check at depth 20 exceed 1.2 times depth 1, or when the job figure at depth 20 exceeds 1.25 times depth 1 in two runs; the count ceilings are the primary guard, the timing a backstop. Stage 3b: stage 4 wires the search to the port (the width and the budget measured again in its shape, `c = 1`; done 2026-11-05, the host and the oracle asking `c = 0`, Table F3); a watchdog on a job's token for D1 (the per-candidate time limit) if Core 4 fails under contention; a file-task pool across jobs if the per-job threads idle (measured: a medium job at W = 1 uses 0.13 of the 12 cores); a sample of the reference check on a twin session in the bench, gate PM (2 against 4 proc-macro processes) and the disjoint-chain shape of M3b were not run, and are run when a result depends on them. | Copy-on-write memo tables (a): bitwise page copies or `Clone` on every RA output, on top of the per-layer stamps (d+) needs anyway. Validity by "no recorded input changed" read on the leaves (d): sound but loses backdating. N databases (b2): every memo recomputed, N full copies. An own analysis engine (c): re-implements RA. Copies of the process by the operating system (copy-on-write `fork`), rejected by the maintainer 2026-11-03: running N forks this way needs hand-made protection around rust-analyzer's internal threads (its dropper threads, the rayon pool, the proc-macro server pipes), the allocator locks and salsa's write that waits for every handle, and that protection breaks on every ra_ap bump; its spike found the proc-macro pipes shared, rayon's pool absent in the child, salsa's write deadlocking when another handle existed at fork time, and RA's dropper threads. A fork crate of salsa in the workspace: RA depends on `salsa` by name. A thread-local carrying the layer, set while the query stack is not empty: RA reads the revision outside queries (`attach`, `new_with`). New ids for re-created tracked structs: ids change and backdating is lost. Inserts into a parent layer after it has children: a child's reads could change under it. The Engine inside `ryo-v2-ra`: the patched salsa and its unsafe would sit beside ryo-v2's rules. A stamp view now (per sealed layer, a map from a memo's address to the newest stamp on the chain): about 56k + 9k·d keys, about double the view bytes, for lookups that already hit in the reading layer or its parent. Stage 3b: an async runtime around the pool (the workspace has none and every port is synchronous; the dedicated workers, the narrow `submit` / `next` / `cancel` handle and withdrawable jobs are kept without it). Forks as copies of the process by the operating system (above). Rayon's global pool for the jobs: it is shared with RA, and its work stealing would nest a job's diagnostic threads where the budget cannot see them. |

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
| 4. move-item's reverse move required a line end right after the moved item; the planter moves the whole line, and an item whose line ends with a `//` comment has its node end before the comment. | `gen-lab/misplace-item-comment-after` (before: no candidate at all, nothing else in its places is offered) | the item runs to its line end when the rest of the line is a `//` comment, which moves with it (`line_end_after_comment`) | `tests/expand_gen.rs` `move_item_collapses_an_item_with_a_trailing_comment`; `expand::tests::a_trailing_line_comment_ends_the_item_at_its_line_end` |

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
(`--outer-check`, 53 s) finds 0 failures in 117 decided candidates (each decision cut to 3 at the
time) and 0 in the 42 first candidates.

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
fails, 0 of the 45 first candidates (section "Outer check"; each decision cut to 3 at the time).


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
copy of the workspace for each `Decided` candidate (every one, or with `--outer-sample N` the top N
of each decision's ranking; the denominator is stated in each row) and counts those that report an
error the unedited workspace does not. Every row below is from the time a decision returned at
most 3 candidates (each decision cut to 3 at the time):

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
candidates are 112 instead of 114: a decision was cut to 3 passing candidates at the time, so where
the rejected move had been one of 3, another passing candidate took its place if there was one (the
harness prints no per-decision count; the 2 fewer are decisions with no such replacement, an
inference). The run had 564 checks (565 before), median warm check 9 ms.

In the second round the one failure is candidate 2 of `shape-lab/inline-variable-ref`, E0507
(a move out of a borrow) at `src/lib.rs:38`, the line of the folded reference: read from the error
and the line, it is the extract of the nested field without its `&` (`let label = job.spec.label;`),
a shape offered before this round, not the new class. rust-analyzer does not report MIR borrow-check
errors (CHECK_LEDGER Gaps G-B-1, whose list of codes does not name E0507 yet); extract-variable
carries the `Borrow` and `Behaviour` risk marks, so such a candidate names `Borrow` in its
`unseen_classes` and carries `Behaviour` in its marks (CHECK_LEDGER M-6). The best (candidate 0)
passes.

On ryo-rs (a verification repository used as a large sample; run 3 of section "ryo-rs (large
run)", before the detectors) 2 of 30 decided candidates (each decision cut to 3 at the time) failed,
in 10 decisions
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
of ryo-rs (the expansion of each planted state over the case's places, every Blueprint applied and
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

## Bench

The M2 measurement of the code state (`ryo-v2 bench --repeat / --large`). Goal: measure
CONCEPT.md Core 4 (5 s or less per candidate on a warm session) by repeating fork, apply,
diagnostics, discard N times on the same candidate, and check whether memory grows linearly (leak
suspect; reported only, not fixed). Decisions D1 to D4 rest on it.

"check" = apply + diagnose (`Event::Checked.check_ms` is the time since apply started). "revert" = discard, plus the post-revert re-diagnosis when assert_revert is on.

### Environment

- date: 2026-10-31
- machine (small run): 12 threads, 30 GiB RAM
- rustc: 1.98.1 (48a229cea 2026-09-01); ra_ap `=0.0.358`
- profile: dev (deps opt-level 2), via `cargo run -q -p ryo-v2-cli` (the package was renamed `ryo-v2-host` afterwards, with the same binary; the commands below use the new name)
- proc-macro: on; assert_revert: on
- `RaConfig.cfg_test`: off. The measurements in this section were taken with it off. Its default is on since D7, and a run with it on diagnoses the test code too, so its numbers are not these.
- another build may run in the same checkout while a run is in progress, which adds noise to wall-clock numbers; the small run is only ~9 s, and no concurrent cargo was checked for.

### Small workspace (measured)

Command (repo root):

```
cargo run -q -p ryo-v2-host -- bench corpus/base/extract-trait-multicrate corpus/broken-01 corpus/broken-04 corpus/valid-02 --repeat 100 --scope both > target/bench-m2/small.log 2> target/bench-m2/small.err
```

Exit 0; 600 run lines, 6 summary lines, all `revert_ok true`. The log was kept locally and is not committed.

#### Table 1: load

Rows named ryo-rs use ryo-rs (a verification repository used as a large sample).

| workspace | local files | load ms | baseline ms (cold) | baseline errors | rss kB after load |
|---|---|---|---|---|---|
| corpus/base/extract-trait-multicrate | 4 | 302 | 3025 | 0 | 522032 |
| ryo-rs, cold load (target empty) | 504 | 28979 | 34768 | 5 | not captured |
| ryo-rs, warm load (target built) | 504 | 3172 | 35911 | 5 | 5017192 |

ryo-rs rows: measured on 16 threads / 30 GB, busy with other builds, dev profile, proc-macro on, assert_revert on, cargo at `-j 6` with a 16 GB memory limit, 2026-10-31 (runs with repeat 1 and repeat 10). The raw logs are not kept in this repo. Cold load includes building ryo-rs's build scripts and proc-macros once; the baseline (diagnosing all 504 files) is about 35 s either way.

#### Table 2: per run (n = 100 per row for the small workspace)

| workspace | variant | scope | scope files | n | first check ms | warm check ms min/med/max | warm revert ms min/med/max | rss kB first/last/max | slope kB/iter | leak suspect |
|---|---|---|---|---|---|---|---|---|---|---|
| small | corpus/broken-01 | affected | 4/4 | 100 | 8 | 2/3/5 | 2/3/5 | 522044/522052/522052 | 0.1 | false |
| small | corpus/broken-01 | workspace | 4/4 | 100 | 4 | 2/2/4 | 2/2/3 | 522052/522052/522052 | 0.0 | false |
| small | corpus/broken-04 | affected | 4/4 | 100 | 7 | 6/7/8 | 6/7/8 | 522052/522052/522052 | 0.0 | false |
| small | corpus/broken-04 | workspace | 4/4 | 100 | 6 | 6/7/9 | 6/7/8 | 522052/522052/522052 | 0.0 | false |
| small | corpus/valid-02 | affected | 4/4 | 100 | 4 | 2/3/6 | 2/3/5 | 522052/522052/522052 | 0.0 | false |
| small | corpus/valid-02 | workspace | 4/4 | 100 | 3 | 2/3/5 | 2/3/5 | 522052/522052/522052 | 0.0 | false |

Notes: `slope` and `leak suspect` are computed over the warm series (run 2 onward). broken-04 edits only `user`, but its Affected scope is also 4/4 here, so Affected and Workspace are not distinguishable on this fixture (the cause, whether the closure genuinely covers all 4 files or `rev_deps` fell back to an empty map, was not investigated).

The run's Core 4 line (printed then as `principle-15:`): `holds (worst warm median check_ms 7 at corpus/broken-04 [affected], limit 5000)`.

### ryo-rs (large)

This run is not part of the default test suite: 227k lines of `.rs` exceeds a small machine's compute budget, so it ran on a larger machine with `cargo run -q -p ryo-v2-cli -- bench --large <BASE> --repeat N --scope both` (ryo-rs exported at 67b0ebfb with `git archive`, not cloned).

Steps for the large run (from the ryo-v2 repo root, so `corpus/body-0N` resolves; `<RYO_RS>` is a checkout of ryo-rs, `<BASE>` an empty directory without a `target` path component; the export is not kept in this repo):

```
git -C <RYO_RS> archive 67b0ebfb | tar -x -C <BASE>
mkdir -p target/bench-m2
# (b0) cold load (target empty) + estimate of cost; this run's [load] is the "cold load (target empty)" row
cargo run -q -p ryo-v2-host -- bench --large <BASE> --repeat 1 --scope both > target/bench-m2/large-r1.log 2>&1
# (b) T = sum of check_ms + revert_ms over all runs in large-r1.log; N = clamp(floor(300000 / T), 2, 10)
#     this run's [load] is the "warm load (target built)" row
cargo run -q -p ryo-v2-host -- bench --large <BASE> --repeat N --scope both > target/bench-m2/large.log 2>&1
# fallbacks if even N = 2 is too slow: add --no-assert-revert (keep b0 with assert on to confirm revert_ok true)
# optional: load only plus repeat 2 without proc-macro
cargo run -q -p ryo-v2-host -- bench --large <BASE> --repeat 2 --scope both --no-proc-macro > target/bench-m2/large-nopm.log 2>&1
```

Before (b), check `pgrep -fa "cargo|rustc"` so that nothing else is building. Afterwards confirm the source repo is unchanged (`git -C <RYO_RS> status --short`).

The rows below are taken from the logs only: Table 2 rows for corpus/body-01 (renames `pub fn parse` in `ryo-symbol`, which 10 crates depend on; Affected is expected to approach Workspace), corpus/body-02, corpus/body-03 (leaf crate `ryo-cli`; if Affected equals Workspace here, suspect `rev_deps.rs` / `cargo metadata` failure).

| workspace | variant | scope | scope files | n | first check ms | warm check ms min/med/max | warm revert ms min/med/max | rss kB first/last/max | slope kB/iter | leak suspect |
|---|---|---|---|---|---|---|---|---|---|---|
| ryo-rs | corpus/body-01 | affected | 434/504 | 10 | 3150 | 1879/2089/2277 | 1950/2078/2240 | 5073748/5319616/5319616 | 19930.7 | false |
| ryo-rs | corpus/body-01 | workspace | 504/504 | 10 | 2516 | 2260/2403/2548 | 1958/2115/2256 | 5321484/5551064/5551064 | 22417.6 | false |
| ryo-rs | corpus/body-02 | affected | 434/504 | 10 | 1453 | 1359/1412/1561 | 1331/1417/1547 | 5551748/5556948/5556948 | 325.3 | false |
| ryo-rs | corpus/body-02 | workspace | 504/504 | 10 | 1471 | 1462/1529/1602 | 1464/1537/1683 | 5556948/5631328/5631328 | 9834.1 | false |
| ryo-rs | corpus/body-03 | affected | 41/504 | 10 | 1908 | 1578/1708/1800 | 1467/1550/1618 | 5631328/5638296/5638296 | 642.5 | false |
| ryo-rs | corpus/body-03 | workspace | 504/504 | 10 | 4093 | 3934/4298/4677 | 1676/1802/2139 | 5661052/5751508/5751508 | 13060.7 | false |

N = 10: repeat 1 gave T = 22.9 s over the 6 rows, so clamp(floor(300000 / 22853), 2, 10) = 10. new_errors per run: body-01 28, body-02 0, body-03 110 (body-03 carries the stale-overlay errors noted under Anomalies; body-01's 28 were not inspected). The Affected scope does narrow on ryo-rs: 41/504 for the leaf crate `ryo-cli` (body-03), 434/504 for `ryo-symbol` (body-01, depended on by most crates). So the 4/4 on the small fixture is the closure genuinely covering it, not a `rev_deps` fallback.

#### ryo-rs without assert_revert (`--no-assert-revert`, repeat 5, HEAD 7b13339)

| variant | scope | warm check ms min/med/max | warm revert ms min/med/max |
|---|---|---|---|
| corpus/body-01 | affected | 2401/2621/3219 | 286/337/490 |
| corpus/body-01 | workspace | 2526/2610/3179 | 335/358/462 |
| corpus/body-02 | affected | 2068/2347/3251 | 270/275/333 |
| corpus/body-02 | workspace | 2537/2744/2921 | 263/299/391 |
| corpus/body-03 | affected | 309/407/596 | 20/39/59 |
| corpus/body-03 | workspace | 2548/2721/4438 | 249/250/389 |

Revert drops from 1.4–2.1 s to 0.04–0.36 s: the cost was the re-diagnosis that the assert does. The check times are not comparable with the repeat-10 table: the machine was busier (baseline 67 s here against 35 s), and body-03 Affected fell to 0.4 s for a reason not investigated. Core 4 line: `holds (worst warm median check_ms 2744 at corpus/body-02 [workspace], limit 5000)`.

### Stacked against rebuilt (D10)

What writing a child on top of its parent (D10) changes against rebuilding it from the base: (a)
time per checked child, (b) time per expanded parent, (c) memory per level, (d) that the results
are equal.

The two builds, both of branch `feat/stacked-search`:

- "rebuilt": dda64c4, the commit that added the measurement fields and `bench --on` and changed
  nothing in the search; the baseline, built in a second worktree at `<WT1>` with its own target
  directory.
- "stacked": eff6820, the commit of the switch. Only its rows carry `depth_writes`.

Run order, back to back after eff6820 on one machine (12 threads, 30 GiB RAM, otherwise idle),
2026-11-02: (1) rebuilt `search-recall`, (2) stacked `search-recall`, (3) `bench --on` with the
stacked build, both modes in one run. `search-recall` is the binary of `ryo-v2-oracle`
(`cargo run -q -p ryo-v2-oracle --bin search-recall --`), `ryo-v2` that of `ryo-v2-host`.

```
git worktree add <WT1> dda64c4
# (1) rebuilt, in <WT1>
search-recall run corpus/search --report <OUT>/rebuilt.json
# (2) stacked, in the checkout at eff6820
search-recall run corpus/search --report <OUT>/stacked.json
# (3) bench --on, stacked build (each variant in modes rebuilt and stacked)
ryo-v2 bench corpus/base/extract-trait-multicrate corpus/broken-01 corpus/broken-04 --on corpus/valid-02 --repeat 100 --no-assert-revert --scope both
```

The ryo-rs runs were handed over with these commands, from the ryo-v2 repo root,
`<BASE>` the ryo-rs export at 67b0ebfb and `<OUT>/ryo-rs` the decision corpus of section "Large
runs" (step 1); N follows step (b) of section "ryo-rs (large)":

```
git worktree add <WT1> dda64c4
# (1) rebuilt, in <WT1>
cargo run -q -p ryo-v2-oracle --bin search-recall -- run <OUT>/ryo-rs --report <OUT>/ryo-rs-rebuilt.json
# (2) stacked, in the checkout at eff6820
cargo run -q -p ryo-v2-oracle --bin search-recall -- run <OUT>/ryo-rs --report <OUT>/ryo-rs-stacked.json
# (3) bench --on, stacked build
cargo run -q -p ryo-v2-host -- bench <BASE> corpus/body-01 corpus/body-03 --on corpus/body-02 --repeat N --no-assert-revert --scope both > <OUT>/large-on.log 2>&1
```

They ran 2026-11-03, back to back on one machine (12 threads, 30.27 GiB RAM by `/proc/meminfo`
MemTotal, otherwise idle), every build a release build (`cargo run -q --release`), on the decision
corpus regenerated by step 1 of section "Large runs" with main 531fc18 (11 decisions, 2 of them
pairs). "Rebuilt" is dda64c4, built in the second worktree `<WT1>`; "stacked" is main 531fc18,
which contains eff6820. Order: (1) rebuilt `search-recall run`, (2) stacked `search-recall run`,
(3) `bench --on` with the stacked build, `--repeat 3`.

#### Table S1: the search (`search-recall run`)

Per corpus and build. Time in checks per checked child is the sum of `depth_check_ms` over the sum
of `depth_checked`; in the stacked build it includes the re-path and the start state's diagnosis.
Time in expansion per parent is the sum of `depth_expand_ms` over the sum of `depth_parents`.
Trigger 2 is the median `Event::Checked` `check_ms` (D4). No outer check was run, so the run ms is
the whole run.

| corpus | build | decisions measured | decisions with a wall-clock effect | rows equal to the other build among the rest | time in checks per checked child ms | time in expansion per parent ms | level writes per checked child | median `Event::Checked` `check_ms` (checks) | median decision ms | run ms without the outer check | RSS kB at the end |
|---|---|---|---|---|---|---|---|---|---|---|---|
| small bases (`corpus/search`) | rebuilt (dda64c4) | 45 | 0 | yes | 10.4 (5,819 ms over 562 children) | 10.5 (880 ms over 84 parents) | not measured (stacked only) | 7 (613 checks) | 150 | 24,565 | 622,972 |
| small bases (`corpus/search`) | stacked (eff6820) | 45 | 0 | yes | 10.2 (5,734 ms over 562 children) | 10.9 (916 ms over 84 parents) | 1.22 (686 writes) | 6 (658 checks) | 171 | 24,673 | 625,620 |
| ryo-rs | rebuilt (dda64c4) | 11 | 0 | yes | 1,139.5 (225,622 ms over 198 children) | 222.5 (4,228 ms over 19 parents) | not measured (stacked only) | 1,007 (209 checks) | 6,791 | 329,634 | 5,554,272 |
| ryo-rs | stacked (main 531fc18) | 11 | 0 | yes | 1,004.1 (198,820 ms over 198 children) | 227.4 (4,320 ms over 19 parents) | 1.14 (225 writes) | 751 (220 checks) | 8,318 | 303,683 | 5,414,800 |

Both builds: 41 hits, recall@beam 0.911. (d): each report holds 45 rows, one per decision, in the same
order (the id `obscure-name-0` names two decisions in different workspaces, so rows are compared by
position). Every non-time field of every row is equal between the builds (generated, hit, best_rank, final_rank,
fate, candidates, kept, depth_checked, depth_parents, kinds, decided, top3, noul_audited and the
others); no row differs, and no decision had a wall-clock effect (no `budget_exhausted`, no
`soft_stopped`, no `TimedOut`).

ryo-rs, both builds: 10 of the 11 decisions generated their best, 9 hits, recall@beam 0.9. (d):
each report holds 11 rows, compared by position; every non-time field of every row is equal
between the builds, no row differs, and no decision had a wall-clock effect. Triggers 1, 2 and 4
fired in neither build; trigger 3 is not measured in either (fewer than 1,000 checks).

#### Table S2: one child, controlled (`bench --on`)

Workspace small (`corpus/base/extract-trait-multicrate`), parent `corpus/valid-02`, repeat 100,
`--no-assert-revert`, the stacked build; same machine and date as S1. Each `a/b/c` triple is
min/median/max as the bench prints it. `rebuilt` verifies the parent's and the variant's edits
from the base (`app::verify`); `stacked` writes the parent as level 1 and diagnoses it once, then
per repeat writes the variant's own edits as level 2, diagnoses (its verdict by
`Verdict::of_level`) and pops.

| workspace | parent | variant | scope | mode | first check ms | warm check ms min/median/max | warm revert ms min/median/max | rss kB min/median/max | rss kB after the parent level |
|---|---|---|---|---|---|---|---|---|---|
| small | corpus/valid-02 | corpus/broken-01 | affected | rebuilt | 13 | 3/4/7 | 0/0/1 | 528,396/528,800/528,800 | not measured (stacked only) |
| small | corpus/valid-02 | corpus/broken-01 | affected | stacked | 8 | 3/3/4 | 0/0/1 | 528,800/528,800/528,800 | 528,800 |
| small | corpus/valid-02 | corpus/broken-01 | workspace | rebuilt | 3 | 3/4/4 | 0/0/1 | 528,800/528,800/528,800 | not measured (stacked only) |
| small | corpus/valid-02 | corpus/broken-01 | workspace | stacked | 7 | 3/3/4 | 0/0/1 | 528,800/528,800/528,800 | 528,800 |
| small | corpus/valid-02 | corpus/broken-04 | affected | rebuilt | 20 | 3/4/5 | 0/0/1 | 528,800/528,800/528,800 | not measured (stacked only) |
| small | corpus/valid-02 | corpus/broken-04 | affected | stacked | 21 | 3/3/4 | 0/0/1 | 528,800/528,800/528,800 | 528,800 |
| small | corpus/valid-02 | corpus/broken-04 | workspace | rebuilt | 4 | 3/4/5 | 0/0/1 | 528,800/528,800/528,800 | not measured (stacked only) |
| small | corpus/valid-02 | corpus/broken-04 | workspace | stacked | 20 | 3/3/4 | 0/0/1 | 528,800/528,800/528,800 | 528,800 |
| ryo-rs | corpus/body-02 | corpus/body-01 | affected | rebuilt | 2332 | 1487/1487/1499 | 177/177/180 | 5,234,960/5,280,772/5,280,772 | not measured (stacked only) |
| ryo-rs | corpus/body-02 | corpus/body-01 | affected | stacked | 2220 | 1410/1410/1519 | 182/182/184 | 5,288,792/5,348,040/5,348,040 | 5,286,472 |
| ryo-rs | corpus/body-02 | corpus/body-01 | workspace | rebuilt | 1469 | 1573/1573/1633 | 211/211/212 | 5,399,888/5,414,320/5,414,320 | not measured (stacked only) |
| ryo-rs | corpus/body-02 | corpus/body-01 | workspace | stacked | 2168 | 1573/1573/1594 | 202/202/212 | 5,425,564/5,436,940/5,436,940 | 5,418,704 |
| ryo-rs | corpus/body-02 | corpus/body-03 | affected | rebuilt | 4853 | 1456/1456/1525 | 178/178/179 | 5,440,584/5,453,508/5,453,508 | not measured (stacked only) |
| ryo-rs | corpus/body-02 | corpus/body-03 | affected | stacked | 1527 | 149/149/157 | 17/17/18 | 5,453,516/5,453,516/5,453,516 | 5,453,516 |
| ryo-rs | corpus/body-02 | corpus/body-03 | workspace | rebuilt | 1644 | 1451/1451/1675 | 196/196/199 | 5,453,516/5,483,236/5,483,236 | not measured (stacked only) |
| ryo-rs | corpus/body-02 | corpus/body-03 | workspace | stacked | 4918 | 1609/1609/1618 | 201/201/202 | 5,514,252/5,514,252/5,514,252 | 5,497,620 |

The scope files, the RSS after load and the slope kB/iter of this run were not taken down. The
broken-04 runs reported 3 new errors in both modes. Core 4 holds: the worst warm median check is 4
ms, at corpus/broken-01 [affected], rebuilt mode.

The ryo-rs rows: `<BASE>` the ryo-rs export at 67b0ebfb, parent `corpus/body-02`, repeat 3,
`--no-assert-revert`, the stacked build (main 531fc18), release; run (3) of S1's ryo-rs runs, same
machine and date, one run. Its load: 1,332 ms for 504 local files, a cold baseline of 43,861 ms with
5 pre-existing errors, 5,169,064 kB RSS after load. The build prints no core occupancy (`bench`
gained it after main 531fc18), so none is recorded. Core 4 holds: `core-4: holds (worst warm median
check_ms 1609 at corpus/body-03 [workspace] mode stacked, limit 5000)`.

Derived, not measured: a `Level` keeps the whole file list of its state (`ryo-v2-ra` `fork.rs`), so
on ryo-rs (504 files) one level holds 504 entries, and a search at `max_depth` 3 holds at most 4
levels (`max_depth` + 1), 2,016 entries.

### Memory composition (engine probe, D11)

Stage 1 of D11: what a loaded session's memory is made of and what one stacked check adds to the
heap, measured by `engine-probe` (`ryo-v2-host`) on the one stacked procedure `bench --on` times
(`stacked::stacked_series`). Memory is in GiB (2^30 bytes) unless a cell names another unit; the
heap is the probe's counting allocator, the RSS is VmRSS.

- date: 2026-11-03; machine: 12 cores, 12 threads (`RaConfig.threads` 12), 30.27 GiB RAM
  (`/proc/meminfo` MemTotal), otherwise idle (the reports' `idle` flag is false: `--idle` was not
  passed).
- build: the probe at 74f3932, release, built with `RUSTFLAGS="--cfg no_salsa_async_drops"`
  (synchronous drops, below).
- small base: `corpus/base/extract-trait-multicrate`, `--on corpus/valid-02`, variant
  `corpus/broken-01`, repeat 3, both scopes.
- ryo-rs: `<BASE>` the ryo-rs export at 67b0ebfb, `--on corpus/body-02`, variants
  `corpus/body-01` and `corpus/body-03`, repeat 5, Affected scope.
- Both runs are smaller than the run the task asked for (small base also
  `corpus/broken-04` and repeat 20; ryo-rs repeat 10 at both scopes).

**Synchronous drops.** In ra_ap 0.0.358 a dropped `Parse` (`ra_ap_syntax`), `AstIdMap` and span
map (`ra_ap_span`) is sent to a dropper thread of its own and freed there, unless the cfg
`no_salsa_async_drops` is set (`#[cfg(not(no_salsa_async_drops))]` on their `Drop`). With those
threads, memory is freed in the background while a check runs, so the per-check delta mixes the
check's own allocations with frees from earlier revisions: in the earlier run of the same small
base without the cfg (the stage-1 finding: the same parent and variant, repeat 3, both
scopes) the delta was +0.26 MiB on the first check and negative on the five later ones (-7.1 to
-15.7 MiB). With synchronous drops the frees happen on the thread that drops the value, within
the operation that drops it, and the delta is positive on every check and stable (Table M3).

#### Table M1: phases

P0 before load, P1 at `Event::Indexed`, P2 at `Event::Loaded` (baseline done), P3 after the last
check.

| base | phase | RSS | live heap | peak heap |
|---|---|---|---|---|
| small | P0 | 0.0057 | 0.0000008 | 0.00002 |
| small | P1 | 0.115 | 0.0548 | 0.0596 |
| small | P2 | 0.495 | 0.394 | 0.395 |
| small | P3 | 0.504 | 0.198 | 0.404 |
| ryo-rs | P0 | 0.0056 | 0.0000009 | 0.00002 |
| ryo-rs | P1 | 0.589 | 0.300 | 0.310 |
| ryo-rs | P2 | 4.908 | 4.016 | 4.035 |
| ryo-rs | P3 | 5.088 | 2.029 | 4.099 |

Between P2 and the first check's `h0` (the parent level written and diagnosed) the live heap falls
from 4.016 to 2.036 GiB on ryo-rs and from 0.394 to 0.199 GiB on the small base; the report does
not say what was freed.

#### Table M2: the largest rows of `memory_report`, at P2

Counts and shallow sizes as salsa's `memory_usage` reports them (fields plus metadata, in MB of
10^6 bytes); the share is of the sum of the ingredient rows or of the memo rows. P3 gives the same
rows within 0.1 percentage point.

| base | rows | sum (count; shallow) | top 3 by count (count, share) | top 3 by shallow size (MB, share) |
|---|---|---|---|---|
| small | ingredients (38) | 94,894; 8.1 MB | `FunctionId` 19,214 (20.2 %), `SyntaxContextImpl` 15,274 (16.1 %), `ImplId` 13,675 (14.4 %) | `MacroCallId` 1.6 (20.4 %), `FunctionId` 1.5 (19.1 %), `SyntaxContextImpl` 1.2 (15.2 %) |
| small | memos (68) | 128,675; 13.6 MB | `AttrFlags::query_` 34,723 (27.0 %), `ImplSignature::of_` 13,675 (10.6 %), `ImplSignature::with_source_map_` 13,675 (10.6 %) | `ImplSignature::with_source_map_` 2.8 (20.5 %), `AttrFlags::query_` 2.3 (16.7 %), `impl_trait_with_diagnostics` 1.5 (10.7 %) |
| ryo-rs | ingredients (49) | 1,055,005; 95.5 MB | `SyntaxContextImpl` 196,744 (18.6 %), `MacroCallId` 192,972 (18.3 %), `ImplId` 127,703 (12.1 %) | `MacroCallId` 29.3 (18.5 fields + 10.8 metadata; 30.7 %), `SyntaxContextImpl` 15.7 (4.7 + 11.0; 16.5 %), `ImplId` 10.2 (3.1 + 7.2; 10.7 %) |
| ryo-rs | memos (100) | 1,503,237; 209.6 MB | `AttrFlags::query_` 187,396 (12.5 %), `ImplSignature::of_` 109,346 (7.3 %), `ImplSignature::with_source_map_` 109,346 (7.3 %) | `InferenceResult < 'db >::for_body_` 34.8 (16.6 %), `ImplSignature::with_source_map_` 22.3 (10.6 %), `impl_trait_with_diagnostics` 13.9 (6.6 %) |

`heap_size_of_fields` is `None` on every row at P2, and `None` or 0 at P3 (0 on six memo rows of
ryo-rs and three of the small base): RA's types declare no heap size, so the report gives shallow
sizes only. The rows' sums are 0.284 GiB of the 4.016 GiB live heap at P2 on ryo-rs (7.1 %) and
0.020 GiB of 0.394 GiB on the small base (5.1 %).

#### Table M3: per check

Per variant and scope, min / median / p90 / max; p90 is the nearest rank (with 5 or 3 checks it is
the largest). Freed at write `h0 - hW`, delta `h1 - hW`, freed at pop `h2 - h3` (`ProbeReport`
C2), read from the counting allocator. The probe's wall ms carry the allocator's overhead and are
no time figure of a check.

| base | variant | scope | checks | freed at write | delta | freed at pop | probe wall ms min/median/max | probe overhead against `bench` |
|---|---|---|---|---|---|---|---|---|
| small | corpus/broken-01 | affected | 3 | 0.2287 / 0.2295 / 0.6978 / 0.6978 MiB | 0.2352 / 0.2352 / 0.2491 / 0.2491 MiB | 0.0044 / 0.0051 / 0.0052 / 0.0052 MiB | 6/7/7 | not measured |
| small | corpus/broken-01 | workspace | 3 | 0.2295 / 0.2295 / 0.3122 / 0.3122 MiB | 0.2352 / 0.2352 / 0.2353 / 0.2353 MiB | 0.0044 / 0.0051 / 0.0052 / 0.0052 MiB | 6/6/7 | not measured |
| ryo-rs | corpus/body-01 | affected | 5 | 0.3330 / 0.3331 / 0.3501 / 0.3501 | 0.3332 / 0.3333 / 0.3384 / 0.3384 | 0.0001 / 0.0001 / 0.0001 / 0.0001 | 3771/3830/3870 | not measured |
| ryo-rs | corpus/body-03 | affected | 5 | 0.0502 / 0.0504 / 0.3338 / 0.3338 | 0.0488 / 0.0489 / 0.0553 / 0.0553 | -0.0018 / -0.0014 / -0.0014 / -0.0014 | 1530/1577/1928 | not measured |

The largest freed at write of each ryo-rs variant is its first check (0.350 and 0.334). On
body-03 the pop frees less than nothing: the heap grows by 0.0014 to 0.0018 GiB across it. The
overhead is not measured: the reports' bench median is empty, and the `bench --on` of section
"Stacked against rebuilt" ran on another build (main 531fc18) and was not run back to back with the
probe.

#### Gates of stage 1 (D11)

Fixed before the run. `RAM` 30.27, `RESERVED` 8; `B` the ryo-rs RSS at P2, 4.908;
`D` the p90 of the delta over the ten ryo-rs checks, 0.3335.

| gate | result |
|---|---|
| G1 memory (lower bound) | not refused: `D` > 0 on every check, and `N_low = floor((30.27 - 8 - 4.908) / 0.3335) = 52` >= 4; under the stricter reading (P3 RSS 5.09, the largest delta 0.338) `floor((30.27 - 8 - 5.09) / 0.338) = 50`. Both are above 16 as well (CONCEPT.md's upper figure, reported, not gated). A lower bound only: the delta omits a layer's stamps, tracked-field values, input overlay and map overhead, and M2 of stage 2 decides the memory |
| G2 composition | not met: P1, P2, P3 RSS and live heap, the freed at write, the delta and the freed at pop are reported for both bases, and the rows of the largest shallow share are named (M2), but salsa's report gives shallow sizes only (`heap_size_of_fields` is `None` for RA's types), so its rows account for 0.284 GiB of the 4.016 GiB live heap on ryo-rs and what holds the rest is not named |
| G3 baseline time | met: the ryo-rs rows of tables S1 and S2 are filled, both builds; S2 is one `bench --on` run with repeat 3 |
| G4 occupancy (reported only) | not measured: the `bench --on` of S2 ran on main 531fc18, which prints no occupancy |

#### Table M4: per layer of the Engine (stage 3b, the source of M)

What one layer of the fork Engine costs, the memory terms of stage 3b's `Forks` and of the bound
stage 4 states as `max_forks`:
`W × transient + Σ over live layers of (kept(class) + view(depth)) ≤ A`. Measured by
`engine-probe --layer-cost` (`ryo-v2-host` `layer_cost`): on one fork of the Engine, 21 levels,
each one write of one file, written on the level below and diagnosed once at Affected scope
before the next is written (the search's shape), after one throwaway level that takes the frees
the load leaves (0.099 GiB on the first diagnosis at 1 thread). Per level: kept = live heap after
its diagnosis less before its write; transient = the most heap live at once during its write and
diagnosis less what it keeps (the gauge's peak is reset before each write); view = the level's
`LayerStats::flat_bytes` once it is sealed and has a child. Item levels append a function
(`stacked::depth_levels`); body levels change the literal one function returns, whose first level
on each file appends it and is not counted. The levels rotate over 4 files.

- date: 2026-11-04; machine: 12 cores, 30.27 GiB RAM; other work ran on the machine (load
  average 0.7 to 4.4), which moves no heap figure.
- build: stage 3b's commits 0, 1 and 1b on c379541 (the tree of main's cc343dd; uncommitted then), release, `RUSTFLAGS="--cfg
  no_salsa_async_drops"` (synchronous drops, as Table M3).
- runs: `engine-probe <BASE> --layer-cost --classes body,item --view-depths 1,10,20 --scope
  affected` with `--depth-files 4` (first4: the first 4 indexed files in path order, all of
  `ryo-agent`, Affected scope 18 files) or `--layer-files
  crates/ryo-symbol/src/id.rs,crates/ryo-source/src/ast.rs,crates/ryo-analysis/src/lib.rs,crates/ryo-storage/src/lib.rs`
  (wide4: files of the crates most others depend on, Affected scope 434, 462, 348 and 147 files),
  each at 12 threads per diagnosis and with `--threads 1`; `<BASE>` the ryo-rs export at 67b0ebfb.

Kept and transient in MB (10^6 bytes), min / p50 / p90 / max (nearest rank) over the counted
levels (17 body, 21 item); view bytes of the level at depth 1 / 10 / 20; the stack is the live heap
once all 21 levels are diagnosed less before the fork. All [measured].

| files | threads | class | kept | transient | view at 1 / 10 / 20 | stack (21 levels) |
|---|---|---|---|---|---|---|
| first4 | 12 | body | 1.892 / 1.955 / 2.017 / 2.018 | 0.128 / 0.213 / 0.294 / 0.305 | 0.003 / 0.379 / 0.379 | 64.1 |
| first4 | 12 | item | 1.547 / 2.125 / 13.977 / 14.008 | 0.107 / 0.312 / 3.465 / 3.504 | 0.003 / 0.379 / 0.379 | 160.4 |
| first4 | 1 | body | -3.283 / 2.059 / 2.086 / 2.120 | 0.050 / 0.061 / 0.070 / 4.086 | 0.003 / 0.379 / 0.379 | 65.7 |
| first4 | 1 | item | -3.169 / 2.115 / 19.140 / 19.170 | 0.089 / 0.135 / 3.951 / 3.972 | 0.003 / 0.379 / 0.379 | 160.8 |
| wide4 | 12 | body | 10.549 / 19.647 / 19.720 / 19.722 | 0.568 / 1.071 / 1.564 / 1.708 | 0.207 / 1.516 / 1.516 | 401.1 |
| wide4 | 12 | item | 20.142 / 30.469 / 47.659 / 47.667 | 1.456 / 1.949 / 9.858 / 9.867 | 0.207 / 1.516 / 1.516 | 742.8 |
| wide4 | 1 | body | 10.088 / 16.931 / 20.179 / 20.182 | 0.239 / 0.627 / 0.631 / 0.631 | 0.207 / 1.516 / 1.516 | 398.2 |
| wide4 | 1 | item | 16.501 / 31.558 / 52.167 / 52.175 | 0.294 / 0.601 / 0.715 / 0.715 | 0.207 / 1.516 / 1.516 | 745.2 |

- Kept follows the file, not the depth: on first4 an item level on a `src/` file keeps 13.9 to
  19.2 MB and one on an `examples/` file about 2.1 MB; on wide4 a body level keeps the same
  figure every time its file comes back (within 0.01 MB over depths 5 to 21), and an item level
  at most 6 % more than the first on its file (28.8 to 30.5 MB on `ryo-symbol`). A body level
  keeps about a tenth (first4 `src/`) to two thirds (wide4) of an item level on the same file.
- The view stops growing at depth 3 here: with 4 files the chain's distinct memo keys stop
  growing once each file was written, and `flat_bytes` is the capacity of the view's maps
  (powers of two). A chain over more distinct files keeps growing (F14 of stage 3b). A
  level's view is built when its child seals it, during the next level's write, so kept(k+1)
  likely holds view(k) already; the bound below adds the view again and errs high by at most
  1.52 MB a layer.
- At 1 thread on first4, the levels on `crates/ryo-agent/examples/lsp_hybrid_test.rs` after its
  first keep -3.08 to -3.28 MB with a transient of 3.89 to 4.09 MB (body level 5; item levels 5,
  9, 13, 17 and 21); at 12 threads they keep about 2 MB. What is freed there is not named.
- Stage 1's "one check keeps 0.049 to 0.338 GiB" (Table M3) was an undo-log check of a corpus
  variant on its parent's level, written in place; it is not a layer's cost and is not M.

The bound with the measured terms [estimated]: `A = 30.27 - 8 - 4.908 = 17.36` GiB; the worst
layer is a wide4 item level at 1 thread, 52.17 MB kept plus 1.52 MB of view; the transient of 12
jobs at 1 thread is at most 12 × 0.71 MB. Disjoint chains of depth 20 then fit up to K = 17
(`17 × 20 × 53.69 MB + 8.6 MB = 18.26 GB ≤ 18.64 GB`); 16 × 20 takes 17.19 GB (16.01 GiB). A
search over body edits at the wide4 p50 (19.65 MB + 1.52 MB) takes 6.8 GB for 16 × 20. Not
counted: the RSS above the live heap (allocator, fragmentation), which the bench's RSS stop
guards, and memos two siblings compute twice, which the kept figure already holds per layer.

### Forks in parallel (D11, stage 3b)

N live forks through the `Forks` port, measured by `bench <BASE> --forks <WIDTHS>`
(`ryo-v2-host` `forks_bench`): one job tree run at every width, a seed job on the base's root,
then 4 levels of 24 sibling jobs, every sibling of a level on the sealed layer of job 0 of the
level below, a level submitted at once when its parent is sealed. A job is one write of one file
(an item edit, a function appended, or a body edit, the literal of the seed's function changed),
rotating over the tree's files. jobs/s run from the first submit of level 1 to the last outcome;
a job's check time is its `Event::Checked` (writes and diagnosis, from its start on a worker). A
width written `<n>c` counts the caller's thread out of the workers' share (`caller_works`, stage
4's shape); the others are the bench's shape (`c = 0`). All [measured] unless marked.

- date: 2026-11-05; machine: 12 logical cores (6 physical × 2 SMT), 30.27 GiB RAM.
- build: release; stage 3b's commits 0 to 4 (HEAD 7bd8641) with the bench of commit 5
  (uncommitted then).
- base: `<BASE>` the ryo-rs export at 67b0ebfb; Affected scope; repeat 3; no cut-off (as W4
  runs), except the Core 4 run; two runs of each set; output under `<OUT>`.
- small set: `crates/ryo-ui/src/main.rs` and `crates/ryo-ui/src/scenario_log.rs` (Affected
  scope 2 files). Medium set: the `src/lib.rs` of
  `ryo-plugin-runtime`, `ryo-server`, `ryo-plugin-loader` and `ryo-plugin-api` (Affected scope 44
  to 47 files).
- widths: 1, 4, 8, 11c, 12 and 16 (capped at W = 12 by `T_w`) at T = 12; on the small set also an
  oversubscribed W = 16 at T = 16 (`--fork-threads 16`, t = 1), one run.

#### Table F1: jobs per second by width

jobs/s of run 1 / run 2, the speedup against W = 1 of the same run beside it; the check p50 is
of the small set, in ms. The widths ran in the same order in every repeat (1, 4, 8, 11c, 12, 16),
after one untimed warm-up at W = 1; the bench now rotates the first width per repeat.

| set | W = 1 | W = 4 | W = 8 | W = 11c | W = 12 | 16 capped at 12 | W = 16, T = 16 |
|---|---|---|---|---|---|---|---|
| small, jobs/s | 3.02 / 3.01 | 10.55 / 10.47 (3.49 / 3.48) | 14.86 / 14.82 (4.91 / 4.93) | 16.47 / 16.49 (5.45 / 5.49) | 17.10 / 17.19 (5.65 / 5.72) | 17.14 / 17.13 (5.67 / 5.70) | 18.24 (6.06) |
| small, check p50 ms | 77 | 82 | 150 | 159 | 165 | 179 | 239 |
| medium, jobs/s | 1.19 / 1.18 | 2.99 / 2.97 (2.51) | 3.65 / 3.56 (3.07 / 3.00) | 3.89 / 3.91 (3.27 / 3.30) | 3.93 / 3.91 (3.30) | 3.92 / 3.91 (3.29 / 3.30) | not run |

- The small set flattens above W ≈ 6 while its check p50 doubles; read as the 6 physical cores'
  SMT siblings sharing their units [estimated, not profiled].
- The medium set gains less: the share of workers busy falls to 0.80, because each level waits
  for its job 0 to be sealed while the level's long jobs finish [estimated]. A medium job at
  W = 1 uses 0.13 of the 12 cores, the file-task pool's revisit figure in D11.

#### Table F2: gates of stage 3b (D11)

Fixed before the run.

| gate | result |
|---|---|
| W4-3b | met: 0 differences in every run; compared were 192 rows of W = 1 (repeats 2 and 3 against repeat 1) and 288 rows per other width against W = 1, and the tree serially on one `Stacked` fork against W = 1 (288 rows, twice). By node: the findings of a `Done`, or the kind and file set of a failure |
| X3b-small (scope < 8 files) | met: W = 4 3.49 and 3.48 times W = 1 (gate 1.5 times) |
| X3b-large (scope ≥ 8 files, reported only) | W = 4 2.51 times, W = 12 3.30 times W = 1 |
| T1-3b | met: serial on `Stacked` (Engine) 3.01 to 3.04 jobs/s, check p50 76 and 72 ms; `Forks` at W = 1 3.01 to 3.02 jobs/s, p50 77 and 78 ms: the pool adds no overhead to the serial path |
| Core 4 under contention | met: separate run, cut-off 5,000 ms, the first 4 indexed files (all of `ryo-agent`, Affected scope 18 files); W = 12 median check 436 ms, max 3,552 ms, 0 cut off, 0 over 5 s; W = 1 median 83 ms |
| M3b | stop never reached: the RSS stop at 23,351,920 kB (RAM less 8 GiB); peak RSS 5,244,224 kB (small set) and 5,742,936 kB (medium set), 5.14 to 5.16 GB after load; the process's peak threads 17 to 32. The disjoint-chain shape of M3b was not run |
| PM | not run (2 against 4 proc-macro processes) |

#### Measured beside stage 3b

- Item edits on `crates/ryo-server/src/lib.rs` take about 5.3 s even at W = 1 with t = 12 (about
  6.8 s of CPU in those 5.3 s), over Core 4's 5 s at every width; body edits on the same file
  take 0.13 s. Traced in stage 4 (gate RS of "The search on N forks (D11, stage 4)"): the
  edited file's own diagnosis on one thread, rust-analyzer's type inference, not the Engine. The
  medium set's seed job alone takes 5.4 to 6.5 s, so Core 4 with the cut-off could not be run on
  that set. (The bench's W4 is against W = 1 and the serial `Stacked` run; the twin-session
  reference is in `tests/forks.rs`. What was not run is in D11's "revisit when".)

### The search on N forks (D11, stage 4)

`search_decision` on the `Forks` port (stage 4, bb1fd6e): each verified node owns its Layer, a
child is a job on its parent's Layer, at most `SEARCH_FORKS` = 4 jobs are submitted and not yet
received (the window W), outcomes are consumed in submission order, and the caller only waits
(c = 0, so t = 12 / W). The gates were fixed before the runs. Every
search-recall run used the default budget with the cut-off on, both builds. "Serial build" is
3fdf0fa (stage 3b merged, the search on one `Stacked` fork, `RaConfig.engine` off by default)
in a second worktree `<WT1>`, run as "undo log" and with `--engine` as "Engine"; "stage 4" is
bb1fd6e at `--width 1` and `--width 4`. A row has a wall-clock effect when it has a stop
(`budget_exhausted`, `soft_stopped`, `memory_stopped`) or a `TimedOut` child.

- date: 2026-11-05; machine: 12 logical cores (6 physical × 2 SMT), 30.27 GiB RAM; other work
  ran on the machine (load average 0.3 to 5.9 at the starts of the runs).
- build: release; the runs of the serial build and of stage 4 back to back, one at a time; the
  runs marked "commit 7" on bb1fd6e with stage 4's commits 6 and 7 (uncommitted then), but for
  `SESSION_RAM_GB`, still 6 in that build: its raise to 7 is derived from the commit-7 run's peak.
- corpora: the small bases (`corpus/search`, 45 decisions) and the ryo-rs decision corpus of
  section "Large runs", step 1, regenerated at 3fdf0fa with the same 11 decision ids (`<BASE>`
  the ryo-rs export at 67b0ebfb; the earlier copy had lost its files).

#### Table S4: the search on N forks against the serial build

Time in checks per checked child = the sum of `DepthStats.check_ms` over the checked children
(on the serial build it holds the re-path writes, on stage 4 it has none); median check = the
median `Event::Checked` (a check's write and diagnosis, without the re-path in both builds). All
[measured].

| corpus | build | rows (with a wall-clock effect) | check ms per child | median check ms | median decision ms | search_ms | recall@20 | peak RSS kB |
|---|---|---|---|---|---|---|---|---|
| small | serial, undo log | 45 (0) | 7.0 | 4.0 | 106 | 18,350 | 0.911 | - |
| small | serial, Engine | 45 (0) | 6.9 | 4.0 | 100 | 17,246 | 0.911 | - |
| small | stage 4, W = 1 | 45 (0) | 6.5 | 4.0 | 115 | 18,534 | 0.911 | 646,372 |
| small | stage 4, W = 4 | 45 (0) | 2.5 | 5.0 | 70 | 15,864 | 0.911 | 649,108 |
| ryo-rs | serial, undo log | 11 (0) | 1,044.7 | 790.5 | 8,737 | 311,223 | 0.900 | - |
| ryo-rs | serial, Engine | 11 (0) | 638.2 | 560.5 | 5,547 | 190,715 | 0.900 | - |
| ryo-rs | stage 4, W = 1 | 11 (0) | 660.4 | 557.0 | 5,766 | 204,568 | 0.900 | 5,723,152 |
| ryo-rs | stage 4, W = 4 | 11 (1) | 447.9 | 1,352 | 4,637 | 155,720 | 0.900 | 5,901,028 |
| ryo-rs | commit 7, W = 4 | 11 (0) | 452.7 | 1,383 | 4,659 | 156,494 | 0.900 | 5,926,836 |

- The serial build's two backends gave equal non-time fields on all 45 and all 11 rows.
- Stage 3a's search T1, the only earlier record of it (the 3a merge message, cc343dd; ryo-rs,
  12 threads, 30 GiB): 900.0 ms per checked child on the Engine against 1,047.0 ms on the undo
  log. Commit 0 here gives 638.2 against 1,044.7 ms on the same corpus regenerated.
- W = 1's search_ms is 7.3 % above the serial Engine run's (204,568 against 190,715 ms
  [derived]) while its median check is equal (557.0 against 560.5 ms) and its time per child
  3.5 % higher. The gap is not traced.
- The one row with a wall-clock effect, inline-variable-0 at W = 4, had one `TimedOut` child
  (Blueprint `00000000-0000-750a-b58b-2368d18a7530`) and no stop; its non-time fields are those
  of W = 1 all the same. In the commit-7 run at W = 4 the same row had no `TimedOut` child, and
  every row's non-time fields equal stage 4's W = 4 run but for that `timed_out`: the cut-off
  depends on the load (decision D1: a check is cut off at a time limit).
- The largest `Event::Checked` at W = 4 was 5,915 ms (stage 4) and 5,554 ms (commit 7), the
  ryo-server item edit of gate RS below.

#### Table F3: gates of stage 4 (D11)

Fixed before the runs.

| gate | criterion | result |
|---|---|---|
| I2 | refused writes 0 and the fingerprint preserved after every run | met: no run poisoned its session; every search's `close` and the end-of-decision `audit_revert` passed in every run above |
| W4-search | rows with no wall-clock effect on either side, every non-time field equal (left out: `depth_writes` and `check_ms`, which no longer hold the re-path) | met: small, 45 of 45 rows compared in each pair (W = 1 against W = 4, and each against the serial Engine and undo-log runs), 0 differences; ryo-rs, W = 1 against the serial runs 11 of 11, W = 4 against W = 1 and the serial runs 10 of 11 (inline-variable-0 left out by W = 4), 0 differences. The oracle's `search_verdicts_equal_the_reference_check` holds (W ∈ {1, 4}) |
| C4-4 | every row, cut-off on, W = 1 against W = 4: the resolution, the ranking and recall equal, the TimedOut ids listed per side; no row differs on the small bases, at most 1 of 11 on ryo-rs (explained by its TimedOut set); median `Event::Checked` ≤ 5,000 ms at W = 4 | met: 0 rows differ in decided, fate, best rank, hit, top-3, final rank, candidates or kept, on either corpus; recall 0.911 and 0.900 on both sides; TimedOut ids: none at W = 1, one at W = 4 (above); median check at W = 4 1,352 ms (ryo-rs) and 5 ms (small). `SEARCH_FORKS` stays 4 |
| T1-search | stage 4 at W = 1 (t = 12) against the serial build: time per checked child ≤ 1.10 × the Engine run and ≤ the undo-log run; median `Event::Checked` ≤ 1.05 × the Engine run | met: 660.4 ms ≤ 702.0 (1.10 × 638.2) and ≤ 1,044.7; median 557.0 ms ≤ 588.5 (1.05 × 560.5) |
| X4 | median decision at W = 4 ≤ 0.67 × W = 1 | **missed on ryo-rs**: 4,637 / 5,766 = 0.80 [derived]; met on the small bases, 70 / 115 = 0.61. Occupancy and the share of time idle at the depth barrier were not measured. The response stands: measure speculative expansion before building it. With the depth barrier and no speculation, a depth's slow child (the item edit of gate RS, 5 to 6 s) holds the next depth, so W = 4 shortens the checks (447.9 against 660.4 ms per child) more than the decisions [estimated, not profiled] |
| M-4 | the RSS stop never reached; peak RSS ≤ RAM − RESERVED; the session's peak and its RSS after `close` measured | met in part: no run set `memory_stopped`; peak RSS at most 5,926,836 kB (5.65 GiB), under 22.27 GiB; the RSS after each decision's close (commit 7, W = 4) 5,416,412 to 5,826,780 kB, rising by about 0.4 GB over the 11 decisions (glibc keeps its peak). The live-Layer peak against L_max was not read on ryo-rs (the bound is pinned by `the_searchs_layers_fit_max_forks`: L_max = 73 at 20 roots and W = 4, against `max_forks(30, 4)` = 294); the expansion phase's own peak was not reported |
| RS | the ryo-server item edit classified | not an Engine regression; H2 (below) |
| Port of the series | the depth gate (warm depth 20 ≤ 1.10 × depth 1) in two runs, and `--layer-cost` within Table M4's ranges, on `Forks` | met in part. The depth gate met narrowly on commit 7's port: `bench <BASE> --depth-sweep 1,20 --repeat 5 --engine`, median (the lower middle, as `bench` takes it) of diagnoses 2 to 5 at depth 20 against depth 1: 75.49 / 68.82 = 1.097 and 79.06 / 72.39 = 1.092 [derived]; the mean of the two middle diagnoses gives 1.111 and 1.100 [derived]. On `Forks` a diagnosis of the top after the first is the top level's job run again on the level below, not a re-diagnosis of the level itself; the stacked fork's warm figure (43 to 45 ms at depth 20, stage 3a) is not comparable. `--layer-cost` ran only on a small base (`corpus/base/breakage-lab`), not on ryo-rs |
| P4 | no test hits its timeout over 3 back-to-back `just check` runs | not run |

**Gate RS: the 5.3 s item edit on `crates/ryo-server/src/lib.rs`.** `bench <BASE> <OUT>/rs-item
<OUT>/rs-body --repeat 5 --scope affected`, where `rs-item` holds the file with one function
appended and `rs-body` the same file with one body changed; median warm `check_ms` (min to max of
the warm runs) [measured]. Runs 1 to 3 and 6 on the serial build, runs 4 and 5 on commit 7.

| run | what | item ms | body ms | tells |
|---|---|---|---|---|
| 1 | undo log against Engine | 5,335 (5,298 to 5,532) against 5,302 (5,298 to 5,330): 0.99× | 271 against 299: 1.10× | the Engine is no slower on the item edit; the body edit's difference is 28 ms |
| 2 | the same, `--no-proc-macro` | 5,177 against 5,106 | 207 against 216: 1.04× | without the proc-macro server the item edit is 2 to 4 % faster on both backends: not H1 |
| 3 | `--forks 1 --fork-paths crates/ryo-server/src/lib.rs --fork-depth 2 --fork-jobs 4`, t = 1 against t = 12 (the tree's 40 jobs, item and body edits mixed) | slowest job 5,699 against 5,151 | median job 420 against 130 | twelve threads save 10 % of the slowest job, an item edit |
| 4 | Engine, gap detectors on against `--no-gap-detectors` | 5,328 (5,309 to 5,371) against 5,335 (5,312 to 5,351) | 265 against 241 | ryo-v2's detectors cost nothing measurable on the item edit (24 ms on the body edit) |
| 5 | Engine, `--file-times` | the edited file alone 5,155 to 5,196 ms of 10,367 to 10,472 ms summed over the 45 files; `crates/ryo-cli/src/client.rs` 4,371 to 4,413 ms on another thread; every other file at most 115 ms | 701 to 719 ms summed; the edited file 67 to 75 ms | the time is the edited file's own diagnosis, one file on one thread, not its 44 dependents |
| 6 | `perf record -g` of run 1 (Engine, item), `pidstat` beside it | 73.7 % of the check phase in `hir_ty` / the next trait solver, 6.2 % in `hir_def` / macro expansion, 4.9 % in salsa (the vendored patch included), 0.1 % in ryo-v2; the proc-macro servers idle; about 1.2 cores busy | | H2 |

Classification (the rule of 2.6): the Engine is 0.99× the undo log and the profile puts most of
the time in `hir_ty` queries of one thread, so it is not an Engine regression. H1 (proc-macro
re-expansion) is ruled out by runs 2 and 6. H2 holds: the item edit changes the crate's `DefMap`,
and the 46 `async fn` bodies of the one file are inferred again on the one thread its file task
runs on. It is a rust-analyzer cost, recorded under D4's perf work; its remedy is intra-file
parallelism, a D4 decision outside stage 4. Run 7 (the item appended to another file of
`ryo-server`) was not run.

**The session's memory (D3, D4 trigger 3).** A session is now its base plus its search's Layers.
The peak RSS of one stage-4 session over the ryo-rs corpus at W = 4 is 5,926,836 kB (5.65 GiB,
commit 7; 5,901,028 kB in stage 4's run), so `SESSION_RAM_GB` = ceil(5.65) + ceil(0.75) = 7
(was 6) and `max_sessions(30)` stays 3 ((30 − 8) / 7). `max_forks(30, 4)` becomes 294 (was 314).
D4's trigger 3 reads the RSS after a decision's `close`, between decisions, against
`SESSION_RAM_GB` GiB (`TRIGGER_RSS_KB` = 7,340,032 kB, was 8 GiB); the largest such reading of
the commit-7 run is 5,826,780 kB after 220 checks, so trigger 3 is not measured (fewer than 1,000
checks).

**The undo log deleted (2026-11-15, 1a9db79).** `search-recall run corpus/search` on the build
that deletes the undo-log backend gives the same 45 rows as the build of c51048f: 0 differing,
recall@20 0.911, verdict adopt. It compares the rows' fields; no time figure is taken from it.
The report has no `build` hash key and `search-recall run --engine` is an unknown argument, two
deviations from the plan of stage 4 (D11).

### Verdict

- CONCEPT.md Core 4: holds on both. Small workspace worst warm median check 7 ms; ryo-rs worst warm median 4298 ms (body-03, Workspace scope), with assert_revert on. The run's line: `holds (worst warm median check_ms 4298 at corpus/body-03 [workspace], limit 5000)`. The margin is small at Workspace scope; Affected keeps every row under 2.1 s. Revert costs about as much again (1.4 to 2.1 s median), so one candidate is check + revert, 3 to 6 s.
- ryo-rs memory: RSS 5.0 GB after load, 5.75 GB after 60 runs. It grows in steps (slope up to 22 MB/iter in the rows that diagnose most files) and flattens where little is diagnosed (body-02 Affected 0.3 MB/iter); leak_suspect is false in every row. Not a confirmed leak; whether it levels off needs a longer run.
- comparison with the earlier prototype of this verifier (a single-purpose rust-analyzer checker that is not part of this repository; its own changelog, 0.2.0, gives a first load of about 25 s and about 100 ms warm per candidate): cold load 29 s is in line; the per-candidate check is 14 to 43 times slower than that 100 ms. Not investigated; candidates are the next trait solver, ra 0.0.343 to 0.0.358, the shared machine, or the prototype's number being for a smaller scope.
- memory: on the small workspace no leak suspect (RSS plateau at about 522 MB, slope 0.0 to 0.1 kB/iter over 100 runs, i.e. a few kB in total). The RSS here is the process only; the separate proc-macro-srv process is not included. glibc malloc does not return freed memory, so a plateau is not a leak, and a rising series would need to be read for linear versus levelling-off trend before calling it a leak. The ryo-rs memory line above is the ryo-rs verdict.

### Why simultaneous forks were not built at first, and how they were

ra's `AnalysisHost` is one mutable salsa DB per Session. `Fork` is not an independent copy; it is a token holding in-place edits and the information to revert them. salsa's `Analysis` snapshot is read-only, and `apply_change` cancels any live snapshot. Holding two forks at once would need either two AnalysisHosts (RSS doubles; at ryo-rs scale that is several GB, as the RSS in Table 1 shows) or a writable fork of salsa, which ra does not provide. So we keep `AnalyzerError::ForkInUse`, and CoW forks are a design topic for M3 and later. Restated 2026-11-01 (D3, D9): this holds for forks inside one process; the forks of CONCEPT.md's model stack their writes on one database (D9), and several live at once are copies of the process. Restated 2026-11-03 (D11): several forks in one process are layers over one frozen database; salsa has no writable fork, so the Engine adds one. Restated 2026-11-15 (D11 stage 4): the fork as a token of in-place edits (the undo log) is deleted; every fork of a session is layers of the Engine, and only the flat reference check writes the database in place.

### Anomalies

- `corpus/body-03` (`crates/ryo-cli/src/commands/graph.rs`) was cut from an older ryo-rs. Besides the intended rename `resolve_symbol` to `resolve_symbol_renamed`, it reverts `connect_or_local` to `connect_or_spawn` (diffed against 67b0ebfb), so it can carry unintended new errors. new_errors are reported as measured; the corpus is not fixed.
- Small workspace: the check time is a few ms because the fixture has 4 files; it says little about ryo-rs scale. The 522 MB RSS after load for a 4-file workspace suggests a large fixed cost in ra (probably sysroot / std); this was not separately measured.

## Perf triggers (decision D4)

The table is filled by every run; the small bases cannot fire trigger 1 to 3 (they are tiny). The
ryo-rs columns are run 1 of section "ryo-rs (large run)" (ryo-rs is a verification repository used
as a large sample; 16 cores, 30 GiB): "before" the first run (the machine otherwise idle), "after"
the run at main fff60ef of its subsection "After the four generation fixes and the containment"
(the machine otherwise running other builds). The "small bases stacked" column is the stacked run
(eff6820) of section Bench, "Stacked against rebuilt", table S1 (12 threads, 30 GiB, otherwise
idle, 2026-11-02); "ryo-rs stacked" is the stacked run (main 531fc18) of the same table on ryo-rs
(12 threads, 30.27 GiB, otherwise idle, release build, 2026-11-03). Under D10, trigger 2 reads the `Event::Checked` of
a child's level and of the start state's diagnosis (D4).

| trigger | limit | small bases | ryo-rs before | ryo-rs after | small bases stacked | ryo-rs stacked |
|---|---|---|---|---|---|---|
| 1 median decision | 300,000 ms | 138 ms (not fired) | 8,986 ms (not fired) | 12,319 ms (not fired) | 171 ms (not fired) | 8,318 ms (not fired) |
| 2 median warm check (Affected) | 5,000 ms | 5 ms over 565 checks (not fired) | 1,476 ms over 172 checks (not fired) | 1,187 ms over 206 checks (not fired) | 6 ms over 658 checks (not fired) | 751 ms over 220 checks (not fired) |
| 3 RSS after 1,000 checks, or check time x1.5 | 8 GiB at the end of the run for the runs of this table (from 2026-11-05: 7 GiB, `SESSION_RAM_GB`, read after a decision's close), or 1,500 permille of the first 100 checks | 595,024 kB after 565 checks; both halves not measured: fewer than 1,000 checks | 5,653,460 kB after 172 checks; both halves not measured: fewer than 1,000 checks | 5,501,608 kB after 206 checks; both halves not measured: fewer than 1,000 checks | 625,620 kB after 658 checks; both halves not measured: fewer than 1,000 checks | 5,414,800 kB after 220 checks; both halves not measured: fewer than 1,000 checks |
| 4 one recall run without its outer check, default budget | 600,000 ms | 14,556 ms (not fired) | 360,538 ms (not fired) | 380,928 ms (not fired) | 24,673 ms (not fired) | 303,683 ms (not fired) |

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
