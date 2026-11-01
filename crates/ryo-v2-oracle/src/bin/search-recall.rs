//! Generates planted cases and measures how often the search keeps the best change (H6).
//!
//! Usage:
//! - `search-recall gen <ws> [--kinds a,b|all] [--seeds N] [--pairs N] [--base PATH] [--out DIR]`
//!   (defaults: all kinds, 3 seeds, no pairs). `<ws>` is a corpus-relative workspace
//!   (`base/breakage-lab`, `search/base/search-lab`), or with `--base` the name of the output
//!   directory (`ryo-rs`, a verification repository used as a large sample). Cases go to `--out`, by default `corpus/search/<ws>`; an `--out` with a
//!   `target` component is refused.
//! - `search-recall run <cases-dir> [--limit N] [--beam N] [--exhaustive] [--outer-check]
//!   [--outer-sample N] [--assert-every N] [--report FILE] [--markdown FILE] [--no-proc-macro]`
//!
//! `run` loads one session per workspace, runs every case of it with the production assert
//! sampling (a diagnostic re-assert every 50th check, decision D2 in SEARCH_LEDGER.md) and prints the recall table.
//! `--beam N` narrows the beam (recall@N): on a small base the default beam of 20 prunes little,
//! so a narrow beam is how the ranking is stressed.
//! `--exhaustive` checks up to 200 children per depth with no wall limit (the full-reward best is
//! then ranked among everything); `--outer-check` runs `cargo check` on every decided candidate in
//! a copy of the workspace (server only), or with `--outer-sample N` (at least 1, for a large
//! run; refused without `--outer-check`) on the top N of each decision's ranking; the report
//! names the sample. The time of that outer check is reported apart and left out of trigger 4,
//! which also judges only runs within the default budget (decision D4 in SEARCH_LEDGER.md).

use std::collections::BTreeMap;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use ryo_v2_core::app::ports::{Analyzer, Event};
use ryo_v2_core::app::search::{ASSERT_REVERT_EVERY, DEFAULT_SEARCH_BUDGET};
use ryo_v2_core::domain::Edit;
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::judge::MetricJudge;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_oracle::cargo_check::{CheckOutput, cargo_check};
use ryo_v2_oracle::materials::Materials;
use ryo_v2_oracle::planted::{PlantKind, generate, generate_pair, read_clean, sites, write_case};
use ryo_v2_oracle::search_recall::{
    ContainedPanics, DecisionRow, OuterSample, PlantedCase, RunInfo, markdown, outer_sample,
    parse_outer_sample, report_json, run_case, terminal,
};
use ryo_v2_ra::{RaAnalyzer, RaConfig, SyntaxAssessor};

const USAGE: &str = "usage: search-recall gen <ws> [--kinds a,b|all] [--seeds N] [--pairs N] [--base PATH] [--out DIR]\n       search-recall run <cases-dir> [--limit N] [--beam N] [--exhaustive] [--outer-check] [--outer-sample N] [--assert-every N] [--report FILE] [--markdown FILE] [--no-proc-macro]";

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().context(USAGE)?;
    let target = args.next().context(USAGE)?;
    let rest: Vec<String> = args.collect();
    match cmd.as_str() {
        "gen" => write_cases(&target, &rest),
        "run" => run(&target, &rest),
        _ => bail!("{USAGE}"),
    }
}

fn has_target_component(p: &Path) -> bool {
    p.components().any(|c| c.as_os_str() == "target")
}

fn write_cases(ws: &str, rest: &[String]) -> Result<()> {
    let m = Materials::find().context("find materials")?;
    let mut kinds: Vec<PlantKind> = PlantKind::ALL.to_vec();
    let mut seeds: u32 = 3;
    let mut pairs: u32 = 0;
    let mut base: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| -> Result<&String> {
            it.next().with_context(|| format!("{name} needs a value"))
        };
        match a.as_str() {
            "--kinds" => {
                let v = value("--kinds")?;
                if v != "all" {
                    kinds = v
                        .split(',')
                        .map(|k| {
                            PlantKind::parse(k.trim())
                                .with_context(|| format!("unknown kind {k:?}"))
                        })
                        .collect::<Result<_>>()?;
                }
            }
            "--seeds" => seeds = value("--seeds")?.parse().context("--seeds is a number")?,
            "--pairs" => pairs = value("--pairs")?.parse().context("--pairs is a number")?,
            "--base" => base = Some(PathBuf::from(value("--base")?)),
            "--out" => out = Some(PathBuf::from(value("--out")?)),
            other => bail!("unknown argument {other:?}"),
        }
    }
    let (base_dir, workspace_value) = match &base {
        Some(b) => {
            let abs = b
                .canonicalize()
                .with_context(|| format!("--base {}", b.display()))?;
            let value = abs.to_string_lossy().into_owned();
            (abs, value)
        }
        None => (m.corpus.join(ws), ws.to_string()),
    };
    let out_dir = match out {
        Some(o) => o,
        None => m
            .corpus
            .join("search")
            .join(ws.strip_prefix("search/").unwrap_or(ws)),
    };
    if has_target_component(&out_dir) {
        bail!(
            "--out {} has a `target` component; ra would index nothing there",
            out_dir.display()
        );
    }
    let files = read_clean(&base_dir).with_context(|| format!("read sources of {ws}"))?;
    let mut written = 0usize;
    for &kind in &kinds {
        let n = sites(kind, &files).len();
        println!("{}: {n} sites", kind.id());
        for seed in 0..seeds {
            let id = format!("{}-{seed}", kind.id());
            match generate(kind, seed, &files) {
                Some(p) => {
                    write_case(&out_dir, &workspace_value, &id, &p)
                        .with_context(|| format!("write {id}"))?;
                    println!(
                        "{id}: {} {}:{} {}",
                        kind.id(),
                        p.sites[0].path,
                        p.sites[0].line,
                        p.notes[0]
                    );
                    written += 1;
                }
                None => println!("{id}: skip"),
            }
        }
    }
    // Pairs: every unordered combination of two kinds, cycled, with a growing seed.
    let mut combos: Vec<(PlantKind, PlantKind)> = Vec::new();
    for (i, &a) in kinds.iter().enumerate() {
        for &b in &kinds[i..] {
            combos.push((a, b));
        }
    }
    let mut pairs_written = 0u32;
    if !combos.is_empty() {
        for n in 0..pairs.saturating_mul(4) {
            if pairs_written >= pairs {
                break;
            }
            let (a, b) = combos[n as usize % combos.len()];
            let seed = n / combos.len() as u32;
            if let Some(p) = generate_pair(a, b, seed, &files) {
                let id = format!("pair-{}-{}-{seed}", a.id(), b.id());
                write_case(&out_dir, &workspace_value, &id, &p)
                    .with_context(|| format!("write {id}"))?;
                println!("{id}: {}", p.notes.join(" | "));
                pairs_written += 1;
                written += 1;
            }
        }
    }
    println!(
        "{written} cases ({pairs_written} pairs) in {}",
        out_dir.display()
    );
    Ok(())
}

/// Resident set size of this process in kB, from `/proc/self/status`.
fn rss_kb() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = text.lines().find(|l| l.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

fn copy_workspace(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("create {}", to.display()))?;
    for entry in std::fs::read_dir(from).with_context(|| format!("read {}", from.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "target" || name == ".git" {
            continue;
        }
        let path = entry.path();
        let dest = to.join(&name);
        if path.is_dir() {
            copy_workspace(&path, &dest)?;
        } else {
            std::fs::copy(&path, &dest).with_context(|| format!("copy {}", path.display()))?;
        }
    }
    Ok(())
}

/// Counts of the outer `cargo check`.
#[derive(Default)]
struct Outer {
    checked: usize,
    failed: usize,
    /// Of the above, the best candidate of each decision.
    best_checked: usize,
    best_failed: usize,
}

impl Outer {
    fn summary(&self, sample: OuterSample) -> String {
        format!(
            "\nOuter cargo check (sample: {sample}): {} of {} checked candidates failed; {} of {} best candidates (first of each decision) failed\n",
            self.failed, self.checked, self.best_failed, self.best_checked
        )
    }
}

/// Runs `cargo check` on a copy of `workspace` with `edits` written; the problems it reports that
/// the unedited copy did not: `"{code} {path}:{line}"` for a coded error and
/// `"<no code> {path}:{line}"` for an error without a code.
fn outer_new_errors(
    workspace: &Path,
    edits: &[Edit],
    baseline: &CheckOutput,
    tmp: &Path,
    n: usize,
) -> Result<Vec<String>> {
    let dir = tmp.join(format!("copy-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    copy_workspace(workspace, &dir)?;
    for e in edits {
        let file = dir.join(e.path.as_str());
        match &e.new_text {
            Some(text) => {
                if let Some(parent) = file.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&file, text)?;
            }
            None => {
                let _ = std::fs::remove_file(&file);
            }
        }
    }
    let found = cargo_check(&dir, &tmp.join("build")).context("outer cargo check")?;
    let _ = std::fs::remove_dir_all(&dir);
    let coded = found
        .errors
        .iter()
        .filter(|d| !baseline.errors.contains(d))
        .map(|d| format!("{} {}:{}", d.code, d.path, d.line));
    let codeless = found
        .codeless
        .iter()
        .filter(|c| !baseline.codeless.contains(c))
        .map(|c| format!("<no code> {}:{}", c.path, c.line));
    Ok(coded.chain(codeless).collect())
}

fn run(root: &str, rest: &[String]) -> Result<()> {
    let m = Materials::find().context("find materials")?;
    let mut limit: Option<usize> = None;
    let mut exhaustive = false;
    let mut beam: Option<u16> = None;
    let mut outer = false;
    let mut outer_sample_n: Option<NonZeroUsize> = None;
    let mut assert_every = NonZeroU32::new(ASSERT_REVERT_EVERY);
    let mut report: Option<PathBuf> = None;
    let mut md: Option<PathBuf> = None;
    let mut proc_macro = m.proc_macro;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| -> Result<&String> {
            it.next().with_context(|| format!("{name} needs a value"))
        };
        match a.as_str() {
            "--limit" => limit = Some(value("--limit")?.parse().context("--limit is a number")?),
            "--exhaustive" => exhaustive = true,
            "--beam" => {
                let n: u16 = value("--beam")?.parse().context("--beam is a number")?;
                if n == 0 {
                    bail!("--beam is at least 1");
                }
                beam = Some(n);
            }
            "--outer-check" => outer = true,
            "--outer-sample" => {
                outer_sample_n =
                    Some(parse_outer_sample(value("--outer-sample")?).map_err(anyhow::Error::msg)?);
            }
            "--assert-every" => {
                let n: u32 = value("--assert-every")?.parse().context("a number")?;
                assert_every = NonZeroU32::new(n);
            }
            "--report" => report = Some(PathBuf::from(value("--report")?)),
            "--markdown" => md = Some(PathBuf::from(value("--markdown")?)),
            "--no-proc-macro" => proc_macro = false,
            other => bail!("unknown argument {other:?}"),
        }
    }
    let sample = match (outer, outer_sample_n) {
        (false, None) => OuterSample::Off,
        (false, Some(_)) => bail!("--outer-sample needs --outer-check"),
        (true, None) => OuterSample::All,
        (true, Some(n)) => OuterSample::Top(n),
    };
    let mut cases = PlantedCase::load_all(Path::new(root), &m.corpus).context("load cases")?;
    if let Some(n) = limit {
        cases.truncate(n);
    }
    if cases.is_empty() {
        bail!("no case under {root}");
    }
    let mut budget = DEFAULT_SEARCH_BUDGET;
    if exhaustive {
        budget.beam = 200;
        budget.max_expand = 200;
        budget.soft_ms = u64::MAX;
        budget.hard_ms = u64::MAX;
    }
    if let Some(b) = beam {
        budget.beam = b;
    }
    println!(
        "search-recall: {} cases, beam {}, max_expand {}, proc-macro {}, assert every {}{}",
        cases.len(),
        budget.beam,
        budget.max_expand,
        if proc_macro { "on" } else { "off" },
        assert_every.map_or_else(|| "never".to_string(), |n| n.to_string()),
        if exhaustive { ", exhaustive" } else { "" }
    );

    let started = Instant::now();
    let clock = Arc::new(MonotonicClock::new());
    let observer = Arc::new(RecordingObserver::default());
    let mut by_ws: BTreeMap<PathBuf, Vec<&PlantedCase>> = BTreeMap::new();
    for c in &cases {
        by_ws.entry(c.workspace.clone()).or_default().push(c);
    }
    let tmp = std::env::temp_dir().join(format!("search-recall-{}", std::process::id()));
    let mut rows: Vec<DecisionRow> = Vec::new();
    let mut outer_counts = Outer::default();
    let mut panics = ContainedPanics::default();
    let mut done = 0usize;
    // The oracle's own `cargo check`, outside every search: trigger 4 leaves it out (D4).
    let mut outer_ms = std::time::Duration::ZERO;
    for (workspace, ws_cases) in &by_ws {
        let analyzer = RaAnalyzer::new(
            RaConfig {
                proc_macro,
                threads: None,
                assert_revert: false,
                gap_detectors: true,
                assert_revert_every: assert_every,
                cfg_test: true,
            },
            clock.clone(),
            observer.clone(),
        )
        .with_check_timeout(Some(DEFAULT_SEARCH_BUDGET.per_candidate_ms));
        let mut session = match analyzer.load(workspace) {
            Ok(s) => Some(s),
            Err(e) => {
                for c in ws_cases {
                    rows.push(DecisionRow::skipped(c, format!("load failed: {e}")));
                }
                done += ws_cases.len();
                continue;
            }
        };
        let _ = observer.take();
        let baseline = if outer {
            let t = Instant::now();
            let base = cargo_check(workspace, &tmp.join("build-base")).unwrap_or_default();
            outer_ms += t.elapsed();
            base
        } else {
            CheckOutput::default()
        };
        for case in ws_cases {
            done += 1;
            let Some(s) = session.as_mut() else {
                rows.push(DecisionRow::skipped(case, "session lost"));
                continue;
            };
            let result = run_case(
                &analyzer,
                &analyzer,
                &SyntaxAssessor,
                &MetricJudge,
                &*clock,
                s,
                case,
                budget,
            );
            let checks: Vec<u64> = observer
                .take()
                .into_iter()
                .filter_map(|ev| match ev {
                    Event::Checked { check_ms, .. } => Some(check_ms),
                    _ => None,
                })
                .collect();
            let mut row = match result {
                Ok(run) => {
                    if outer {
                        let checked = outer_sample(&run.decided_edits, sample);
                        for (nth, edits) in checked.iter().enumerate() {
                            outer_counts.checked += 1;
                            outer_counts.best_checked += usize::from(nth == 0);
                            let t = Instant::now();
                            let new = outer_new_errors(
                                workspace,
                                edits,
                                &baseline,
                                &tmp,
                                outer_counts.checked,
                            );
                            outer_ms += t.elapsed();
                            match new {
                                Ok(new) if new.is_empty() => {}
                                Ok(new) => {
                                    outer_counts.failed += 1;
                                    outer_counts.best_failed += usize::from(nth == 0);
                                    let shown: Vec<&str> =
                                        new.iter().take(3).map(String::as_str).collect();
                                    eprintln!(
                                        "outer check failed: {}/{} candidate {nth}: {}",
                                        case.ws,
                                        case.id,
                                        shown.join(", ")
                                    );
                                }
                                Err(e) => eprintln!("outer check of {}: {e:#}", case.id),
                            }
                        }
                    }
                    run.row
                }
                Err(e) => {
                    // The session may be poisoned: start from a fresh one.
                    panics.absorb(&case.ws, &s.detector_panics(), &s.expander_panics());
                    session = analyzer.load(workspace).ok();
                    let _ = observer.take();
                    DecisionRow::skipped(case, format!("search error: {e}"))
                }
            };
            row.check_ms = checks;
            if let Some(s) = session.as_ref() {
                panics.absorb(&case.ws, &s.detector_panics(), &s.expander_panics());
            }
            eprintln!(
                "[{done}/{}] {}/{}: {}",
                cases.len(),
                row.ws,
                row.id,
                match (&row.skipped, row.generated, row.hit) {
                    (Some(why), _, _) => format!("skipped ({why})"),
                    (None, false, _) => "best not generated".to_string(),
                    (None, true, true) => format!("hit (rank {:?})", row.best_rank),
                    (None, true, false) => format!("miss ({})", row.fate.as_deref().unwrap_or("-")),
                }
            );
            rows.push(row);
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    let triggers = RunInfo::of_run(budget, started.elapsed(), outer_ms, sample, rss_kb());
    let mut text = terminal(&rows, triggers);
    if outer {
        text.push_str(&outer_counts.summary(sample));
    }
    let panic_text = format!("\n{}\n", panics.lines().join("\n"));
    text.push_str(&panic_text);
    println!("{text}");
    if let Some(path) = report {
        let mut json = report_json(&rows, triggers);
        json["contained_panics"] = panics.to_json();
        let json = serde_json::to_string_pretty(&json)?;
        std::fs::write(&path, json + "\n").with_context(|| format!("write {}", path.display()))?;
    }
    if let Some(path) = md {
        let mut body = markdown(&rows, triggers);
        if outer {
            body.push_str(&outer_counts.summary(sample));
        }
        body.push_str(&panic_text);
        std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}
