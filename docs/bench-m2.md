# M2: measuring CodeState (`ryo-v2 bench --repeat / --large`)

Goal: measure Vision principle 15 ("inner verification is equivalent to 5 seconds or less per candidate") by repeating fork, apply, diagnostics, discard N times on the same candidate, and check whether memory grows linearly (leak suspect; reported only, not fixed).

"check" = apply + diagnose (`Event::Checked.check_ms` is the time since apply started). "revert" = discard, plus the post-revert re-diagnosis when assert_revert is on.

## Environment

- date: 2026-10-31
- machine (small run): 12 threads, 30 GiB RAM
- rustc: 1.98.1 (48a229cea 2026-09-01); ra_ap `=0.0.358`
- profile: dev (deps opt-level 2), via `cargo run -q -p ryo-v2-cli`
- proc-macro: on; assert_revert: on
- another build may run in the same checkout while a run is in progress, which adds noise to wall-clock numbers; the small run is only ~9 s, and no concurrent cargo was checked for.

## Small workspace (measured)

Command (repo root):

```
cargo run -q -p ryo-v2-cli -- bench corpus/base/extract-trait-multicrate corpus/broken-01 corpus/broken-04 corpus/valid-02 --repeat 100 --scope both > target/bench-m2/small.log 2> target/bench-m2/small.err
```

Exit 0; 600 run lines, 6 summary lines, all `revert_ok true`. The log was kept locally and is not committed.

### Table 1: load

Rows named ryo-rs use ryo-rs (a verification repository used as a large sample).

| workspace | local files | load ms | baseline ms (cold) | baseline errors | rss kB after load |
|---|---|---|---|---|---|
| corpus/base/extract-trait-multicrate | 4 | 302 | 3025 | 0 | 522032 |
| ryo-rs, cold load (target empty) | 504 | 28979 | 34768 | 5 | not captured |
| ryo-rs, warm load (target built) | 504 | 3172 | 35911 | 5 | 5017192 |

ryo-rs rows: measured on a 16 vCPU / 30 GB machine shared with other builds, dev profile, proc-macro on, assert_revert on, cargo capped at 6 jobs / 16G, 2026-10-31 (runs with repeat 1 and repeat 10). The raw logs are not kept in this repo. Cold load includes building ryo-rs's build scripts and proc-macros once; the baseline (diagnosing all 504 files) is about 35 s either way.

### Table 2: per run (n = 100 per row for the small workspace)

| workspace | variant | scope | scope files | n | first check ms | warm check ms min/med/max | warm revert ms min/med/max | rss kB first/last/max | slope kB/iter | leak suspect |
|---|---|---|---|---|---|---|---|---|---|---|
| small | corpus/broken-01 | affected | 4/4 | 100 | 8 | 2/3/5 | 2/3/5 | 522044/522052/522052 | 0.1 | false |
| small | corpus/broken-01 | workspace | 4/4 | 100 | 4 | 2/2/4 | 2/2/3 | 522052/522052/522052 | 0.0 | false |
| small | corpus/broken-04 | affected | 4/4 | 100 | 7 | 6/7/8 | 6/7/8 | 522052/522052/522052 | 0.0 | false |
| small | corpus/broken-04 | workspace | 4/4 | 100 | 6 | 6/7/9 | 6/7/8 | 522052/522052/522052 | 0.0 | false |
| small | corpus/valid-02 | affected | 4/4 | 100 | 4 | 2/3/6 | 2/3/5 | 522052/522052/522052 | 0.0 | false |
| small | corpus/valid-02 | workspace | 4/4 | 100 | 3 | 2/3/5 | 2/3/5 | 522052/522052/522052 | 0.0 | false |

Notes: `slope` and `leak suspect` are computed over the warm series (run 2 onward). broken-04 edits only `user`, but its Affected scope is also 4/4 here, so Affected and Workspace are not distinguishable on this fixture (the cause, whether the closure genuinely covers all 4 files or `rev_deps` fell back to an empty map, was not investigated).

principle-15 line of the run: `principle-15: holds (worst warm median check_ms 7 at corpus/broken-04 [affected], limit 5000)`.

## ryo-rs (large)

This run is not part of the default test suite: 227k lines of `.rs` exceeds a small machine's compute budget, so it ran on a larger shared machine with `cargo run -q -p ryo-v2-cli -- bench --large <BASE> --repeat N --scope both` (ryo-rs exported at 67b0ebfb with `git archive`, not cloned).

Steps for the large run (from the ryo-v2 repo root, so `corpus/body-0N` resolves; `<CLONE>` is any path; the clone is not kept in this repo):

```
git clone --quiet /path/to/ryo-rs <CLONE>
git -C <CLONE> checkout --quiet 67b0ebfb
mkdir -p target/bench-m2
# (b0) cold load (target empty) + estimate of cost; this run's [load] is the "cold load (target empty)" row
cargo run -q -p ryo-v2-cli -- bench --large <CLONE> --repeat 1 --scope both > target/bench-m2/large-r1.log 2>&1
# (b) T = sum of check_ms + revert_ms over all runs in large-r1.log; N = clamp(floor(300000 / T), 2, 10)
#     this run's [load] is the "warm load (target built)" row
cargo run -q -p ryo-v2-cli -- bench --large <CLONE> --repeat N --scope both > target/bench-m2/large.log 2>&1
# fallbacks if even N = 2 is too slow: add --no-assert-revert (keep b0 with assert on to confirm revert_ok true)
# optional: load only plus repeat 2 without proc-macro
cargo run -q -p ryo-v2-cli -- bench --large <CLONE> --repeat 2 --scope both --no-proc-macro > target/bench-m2/large-nopm.log 2>&1
```

Before (b), check `pgrep -fa "cargo|rustc"` so that nothing else is building. Afterwards confirm the source repo is unchanged (`git -C /path/to/ryo-rs status --short`).

Results to fill (from the logs only): N chosen and reason, the two ryo-rs rows of Table 1, and Table 2 rows for corpus/body-01 (renames `pub fn parse` in `ryo-symbol`, which 10 crates depend on; Affected is expected to approach Workspace), corpus/body-02, corpus/body-03 (leaf crate `ryo-cli`; if Affected equals Workspace here, suspect `rev_deps.rs` / `cargo metadata` failure).

| workspace | variant | scope | scope files | n | first check ms | warm check ms min/med/max | warm revert ms min/med/max | rss kB first/last/max | slope kB/iter | leak suspect |
|---|---|---|---|---|---|---|---|---|---|---|
| ryo-rs | corpus/body-01 | affected | 434/504 | 10 | 3150 | 1879/2089/2277 | 1950/2078/2240 | 5073748/5319616/5319616 | 19930.7 | false |
| ryo-rs | corpus/body-01 | workspace | 504/504 | 10 | 2516 | 2260/2403/2548 | 1958/2115/2256 | 5321484/5551064/5551064 | 22417.6 | false |
| ryo-rs | corpus/body-02 | affected | 434/504 | 10 | 1453 | 1359/1412/1561 | 1331/1417/1547 | 5551748/5556948/5556948 | 325.3 | false |
| ryo-rs | corpus/body-02 | workspace | 504/504 | 10 | 1471 | 1462/1529/1602 | 1464/1537/1683 | 5556948/5631328/5631328 | 9834.1 | false |
| ryo-rs | corpus/body-03 | affected | 41/504 | 10 | 1908 | 1578/1708/1800 | 1467/1550/1618 | 5631328/5638296/5638296 | 642.5 | false |
| ryo-rs | corpus/body-03 | workspace | 504/504 | 10 | 4093 | 3934/4298/4677 | 1676/1802/2139 | 5661052/5751508/5751508 | 13060.7 | false |

N = 10: repeat 1 gave T = 22.9 s over the 6 rows, so clamp(floor(300000 / 22853), 2, 10) = 10. new_errors per run: body-01 28, body-02 0, body-03 110 (body-03 carries the stale-overlay errors noted under Anomalies; body-01's 28 were not inspected). The Affected scope does narrow on ryo-rs: 41/504 for the leaf crate `ryo-cli` (body-03), 434/504 for `ryo-symbol` (body-01, depended on by most crates). So the 4/4 on the small fixture is the closure genuinely covering it, not a `rev_deps` fallback.

### ryo-rs without assert_revert (`--no-assert-revert`, repeat 5, HEAD 7b13339)

| variant | scope | warm check ms min/med/max | warm revert ms min/med/max |
|---|---|---|---|
| corpus/body-01 | affected | 2401/2621/3219 | 286/337/490 |
| corpus/body-01 | workspace | 2526/2610/3179 | 335/358/462 |
| corpus/body-02 | affected | 2068/2347/3251 | 270/275/333 |
| corpus/body-02 | workspace | 2537/2744/2921 | 263/299/391 |
| corpus/body-03 | affected | 309/407/596 | 20/39/59 |
| corpus/body-03 | workspace | 2548/2721/4438 | 249/250/389 |

Revert drops from 1.4–2.1 s to 0.04–0.36 s: the cost was the re-diagnosis that the assert does. The check times are not comparable with the repeat-10 table: the machine was busier (baseline 67 s here against 35 s), and body-03 Affected fell to 0.4 s for a reason not investigated. `principle-15: holds (worst warm median check_ms 2744 at corpus/body-02 [workspace], limit 5000)`.

## Verdict

- principle 15: holds on both. Small workspace worst warm median check 7 ms; ryo-rs worst warm median 4298 ms (body-03, Workspace scope), with assert_revert on. The run's line: `principle-15: holds (worst warm median check_ms 4298 at corpus/body-03 [workspace], limit 5000)`. The margin is small at Workspace scope; Affected keeps every row under 2.1 s. Revert costs about as much again (1.4 to 2.1 s median), so one candidate is check + revert, 3 to 6 s.
- ryo-rs memory: RSS 5.0 GB after load, 5.75 GB after 60 runs. It grows in steps (slope up to 22 MB/iter in the rows that diagnose most files) and flattens where little is diagnosed (body-02 Affected 0.3 MB/iter); leak_suspect is false in every row. Not a confirmed leak; whether it levels off needs a longer run.
- comparison with the earlier prototype of this verifier (a single-purpose rust-analyzer checker that is not part of this repository; its own changelog, 0.2.0, gives a first load of about 25 s and about 100 ms warm per candidate): cold load 29 s is in line; the per-candidate check is 14 to 43 times slower than that 100 ms. Not investigated; candidates are the next trait solver, ra 0.0.343 to 0.0.358, the shared machine, or the prototype's number being for a smaller scope.
- memory: on the small workspace no leak suspect (RSS plateau at about 522 MB, slope 0.0 to 0.1 kB/iter over 100 runs, i.e. a few kB in total). The RSS here is the process only; the separate proc-macro-srv process is not included. glibc malloc does not return freed memory, so a plateau is not a leak, and a rising series would need to be read for linear versus levelling-off trend before calling it a leak. The ryo-rs memory line above is the ryo-rs verdict.

## Why simultaneous forks were not built

ra's `AnalysisHost` is one mutable salsa DB per Session. `Fork` is not an independent copy; it is a token holding in-place edits and the information to revert them. salsa's `Analysis` snapshot is read-only, and `apply_change` cancels any live snapshot. Holding two forks at once would need either two AnalysisHosts (RSS doubles; at ryo-rs scale that is several GB, as the RSS in Table 1 shows) or a writable fork of salsa, which ra does not provide. So we keep `AnalyzerError::ForkInUse`, and CoW forks are a design topic for M3 and later.

## Anomalies

- `corpus/body-03` (`crates/ryo-cli/src/commands/graph.rs`) was cut from an older ryo-rs. Besides the intended rename `resolve_symbol` to `resolve_symbol_renamed`, it reverts `connect_or_local` to `connect_or_spawn` (diffed against 67b0ebfb), so it can carry unintended new errors. Write the measured new_errors as they are; the corpus is not fixed.
- Small workspace: the check time is a few ms because the fixture has 4 files; it says little about ryo-rs scale. The 522 MB RSS after load for a 4-file workspace suggests a large fixed cost in ra (probably sysroot / std); this was not separately measured.
