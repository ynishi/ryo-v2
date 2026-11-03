//! The `bench` command body: load once, then repeat fork, apply, diagnose, discard.
//!
//! With `--on <parent overlay>` it measures one child two ways: `rebuilt`, the parent's edits
//! composed with the variant's and verified from the base, and `stacked`, the variant's edits
//! that the parent does not already make, written as a level on top of the parent's level of one
//! `Stacked` fork, by `stacked::stacked_series`.
//!
//! Memory and CPU time are read through `proc.rs`: a failed read is an error of the run, so the
//! host runs on Linux only. With `--on` every run also prints its process CPU ms, its wall ms
//! and its core occupancy ([`occupancy`]).
//!
//! With `--engine` the forks run on layers of the fork Engine (`RaConfig.engine`, D11) instead
//! of the stacked undo log; every summary line ends with ` backend engine` or ` backend undo-log`
//! ([`Backend`]).
//!
//! With `--depth-sweep` ([`run_depth_sweep`]) it measures how a diagnosis of the top level grows
//! with the depth of a stack: per depth K, K one-file levels written on one fork by
//! `stacked::depth_series`, the top level diagnosed `--repeat` times, and the levels popped.

use crate::proc::{self, ProcError};
use crate::stacked::{self, DepthPoint, Point};
use anyhow::{Context, Result};
use ryo_v2_core::app::ports::{Analyzer, AnalyzerError, Event, Observer, Verifier};
use ryo_v2_core::app::verify::verify;
use ryo_v2_core::domain::search::compose;
use ryo_v2_core::domain::{Edit, RelPath, Scope, Verdict};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_core::infra::overlay::overlay_edits;
use ryo_v2_ra::{Backend, RaAnalyzer, RaConfig};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

/// Arguments of one bench invocation.
pub struct BenchArgs {
    /// The workspace to load.
    pub base: PathBuf,
    /// Overlay dirs; each .rs that differs from (or is missing in) `base` is an edit.
    pub variants: Vec<PathBuf>,
    /// The parent overlay of `--on`: each variant is measured as a child of it, rebuilt from the
    /// base and stacked on the parent's level.
    pub on: Option<PathBuf>,
    /// Runs per (variant, scope).
    pub repeat: u32,
    /// Scopes to diagnose, in order.
    pub scopes: Vec<Scope>,
    /// Whether to start the proc-macro server.
    pub proc_macro: bool,
    /// Whether to re-diagnose after the revert and compare with the baseline.
    pub assert_revert: bool,
    /// The backend of the forks (`RaConfig.engine`); every summary line names it.
    pub backend: Backend,
}

/// How a child is brought to its state.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The composed edits of the parent and the child, verified from the base.
    Rebuilt,
    /// The child's edits as a level on top of the parent's level.
    Stacked,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Rebuilt => "rebuilt",
            Mode::Stacked => "stacked",
        }
    }
}

/// What one run reported.
#[derive(Default)]
struct RunStats {
    scope_files: usize,
    total_files: usize,
    check_ms: u64,
    revert_ms: u64,
    revert_ok: Option<bool>,
    rss_kb: Option<u64>,
    /// The run's CPU and wall time; measured with `--on` only.
    usage: Option<Usage>,
}

/// The process CPU time and the wall time of one run, from its write to the end of its revert.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Usage {
    cpu_ms: u64,
    wall_ms: f64,
}

/// A run's start: the process CPU ms and the instant.
type Start = (u64, Instant);

/// Reads the CPU time of this process: `proc::cpu_ms`, or a failing reader in a test.
type CpuReader = fn() -> Result<u64, ProcError>;

/// Starts measuring a run's usage.
fn start(cpu: CpuReader) -> Result<Start, ProcError> {
    Ok((cpu()?, Instant::now()))
}

/// The usage of a run started at `started`, ending now.
fn finish(cpu: CpuReader, (cpu0, t0): Start) -> Result<Usage, ProcError> {
    let cpu1 = cpu()?;
    Ok(Usage {
        cpu_ms: cpu1.saturating_sub(cpu0),
        wall_ms: t0.elapsed().as_secs_f64() * 1000.0,
    })
}

/// The core occupancy of a run: the share of `threads` cores its process kept busy.
///
/// # Contract
/// - C1: a check's occupancy is its process CPU ms over its wall ms times the configured threads;
///   a failed CPU read is an error of the run, not a missing value.
///   tested: bench::tests::occupancy_is_cpu_over_wall_times_threads
pub fn occupancy(cpu_ms: u64, wall_ms: f64, threads: usize) -> f64 {
    cpu_ms as f64 / (wall_ms * threads as f64)
}

/// The result of one run: its events, and its verdict unless the revert failed.
struct Once {
    stats: RunStats,
    label: &'static str,
    verdict: Option<Verdict>,
}

/// The measured series of one (variant, scope).
#[derive(Default)]
struct Series {
    check_ms: Vec<u64>,
    revert_ms: Vec<u64>,
    rss_kb: Vec<u64>,
    occupancy: Vec<f64>,
}

/// Where a series is printed: its variant, scope and mode, and the number of repeats.
struct Heading<'a> {
    name: &'a str,
    scope: &'static str,
    /// The mode as ` mode rebuilt`; empty without `--on`, so that the lines are those of a bench
    /// without it.
    mode: &'a str,
    n_edits: usize,
    repeat: u32,
    /// The session's diagnostic threads with `--on`, where each run's usage is measured;
    /// `None` without it.
    threads: Option<usize>,
}

/// The scope's name, as the bench prints it.
pub fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::Affected => "affected",
        Scope::Workspace => "workspace",
    }
}

fn triple(xs: &[u64]) -> String {
    match summarize(xs) {
        Some((a, b, c)) => format!("{a}/{b}/{c}"),
        None => "-".to_string(),
    }
}

/// Records the VmRSS of this process as an `Event::Memory`; a failed read is the error.
fn sample_memory(observer: &RecordingObserver) -> Result<(), ProcError> {
    let rss_kb = proc::vm_rss_kb()?;
    observer.observe(Event::Memory { rss_kb });
    Ok(())
}

/// The label of a run: PASS or FAIL by its verdict, REVERT-MISMATCH without one.
pub fn label_of(verdict: Option<&Verdict>) -> &'static str {
    match verdict {
        Some(v) if v.passed() => "PASS",
        Some(_) => "FAIL",
        None => "REVERT-MISMATCH",
    }
}

/// The numbers of one run from the events it made.
fn read_events(events: Vec<Event>) -> RunStats {
    let mut stats = RunStats::default();
    for event in events {
        match event {
            Event::Checked {
                scope_files,
                total_files,
                check_ms,
            } => {
                stats.scope_files = scope_files;
                stats.total_files = total_files;
                stats.check_ms = check_ms;
            }
            Event::Reverted {
                revert_ms,
                revert_ok,
            } => {
                stats.revert_ms = revert_ms;
                stats.revert_ok = revert_ok;
            }
            Event::Memory { rss_kb } => stats.rss_kb = Some(rss_kb),
            Event::Indexed { .. } | Event::Loaded { .. } => {}
        }
    }
    stats
}

/// One `verify` of `edits` from the base; with `measure`, its usage from before the `verify`
/// to after it.
fn rebuilt_once<A: Analyzer>(
    analyzer: &A,
    session: &mut A::Session,
    observer: &RecordingObserver,
    edits: &[Edit],
    scope: Scope,
    measure: bool,
) -> Result<Once> {
    observer.take();
    let started = if measure {
        Some(start(proc::cpu_ms)?)
    } else {
        None
    };
    let result = verify(analyzer, session, edits, scope);
    let usage = match started {
        Some(s) => Some(finish(proc::cpu_ms, s)?),
        None => None,
    };
    sample_memory(observer)?;
    let mut stats = read_events(observer.take());
    stats.usage = usage;
    let verdict = match result {
        Ok(v) => Some(v),
        Err(AnalyzerError::RevertMismatch) => None,
        Err(e) => return Err(e.into()),
    };
    Ok(Once {
        stats,
        label: label_of(verdict.as_ref()),
        verdict,
    })
}

/// The bench's hook on `stacked::stacked_series`. It reads VmRSS after the parent's level and,
/// at the end of each repeat, the repeat's events with a VmRSS read, and prints the repeat as a
/// rebuilt run is printed. A failed read is the hook's error, so it ends the series.
struct StackedRecord<'a> {
    observer: &'a RecordingObserver,
    heading: &'a Heading<'a>,
    series: &'a mut Series,
    /// The repeats recorded so far.
    runs: u32,
    /// The VmRSS after the parent's level, once it is diagnosed.
    rss_parent: Option<u64>,
    /// The CPU reader of the repeats' usage, measured when the heading has threads.
    cpu: CpuReader,
    /// The start of the current repeat's usage.
    started: Option<Start>,
}

impl StackedRecord<'_> {
    fn observe(&mut self, point: Point<'_>) -> Result<(), ProcError> {
        match point {
            Point::ParentDiagnosed => {
                self.rss_parent = Some(proc::vm_rss_kb()?);
                self.observer.take();
            }
            // A repeat's events are those from its write on, and so is its usage.
            Point::BeforeWrite => {
                self.observer.take();
                if self.heading.threads.is_some() {
                    self.started = Some(start(self.cpu)?);
                }
            }
            Point::AfterWrite | Point::AfterDiagnose | Point::BeforePop => {}
            Point::AfterPop(verdict) => {
                let usage = match self.started.take() {
                    Some(s) => Some(finish(self.cpu, s)?),
                    None => None,
                };
                sample_memory(self.observer)?;
                let mut stats = read_events(self.observer.take());
                stats.usage = usage;
                self.runs += 1;
                record(
                    self.heading,
                    self.series,
                    self.runs,
                    &stats,
                    label_of(verdict),
                    verdict,
                );
            }
        }
        Ok(())
    }
}

/// Runs `f` with the hook `run` passes to `stacked::stacked_series` for a series of `repeat`
/// repeats; returns what `f` returns, the runs the hook recorded, the VmRSS reads among them and
/// the VmRSS it read after the parent's level.
#[cfg(test)]
pub(crate) fn with_bench_hook<R>(
    repeat: u32,
    f: impl FnOnce(&mut dyn FnMut(Point<'_>) -> Result<(), ProcError>) -> R,
) -> (R, usize, usize, Option<u64>) {
    let observer = RecordingObserver::default();
    let heading = Heading {
        name: "fake",
        scope: "affected",
        mode: " mode stacked",
        n_edits: 1,
        repeat,
        threads: Some(1),
    };
    let mut series = Series::default();
    let mut rec = StackedRecord {
        observer: &observer,
        heading: &heading,
        series: &mut series,
        runs: 0,
        rss_parent: None,
        cpu: proc::cpu_ms,
        started: None,
    };
    let out = f(&mut |point| rec.observe(point));
    let rss_parent = rec.rss_parent;
    (out, series.check_ms.len(), series.rss_kb.len(), rss_parent)
}

/// Runs `heading.repeat` times, prints each run and adds it to `series`. Returns false at the
/// first run whose revert failed.
fn repeat(
    heading: &Heading<'_>,
    series: &mut Series,
    mut once: impl FnMut() -> Result<Once>,
) -> Result<bool> {
    for i in 1..=heading.repeat {
        let Once {
            stats,
            label,
            verdict,
        } = once()?;
        record(heading, series, i, &stats, label, verdict.as_ref());
        if verdict.is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Prints run `i` of a series and adds it to `series`.
fn record(
    heading: &Heading<'_>,
    series: &mut Series,
    i: u32,
    stats: &RunStats,
    label: &str,
    verdict: Option<&Verdict>,
) {
    let Heading {
        name,
        scope,
        mode,
        n_edits,
        repeat: total,
        threads,
    } = *heading;
    let ok = match stats.revert_ok {
        Some(b) => b.to_string(),
        None => "-".to_string(),
    };
    let new = verdict.map_or(0, |v| {
        v.new_errors.errors.len() + v.new_errors.unmapped.len()
    });
    let rss_txt = stats.rss_kb.map_or("-".to_string(), |k| k.to_string());
    // Additive: the columns of a run without `--on` end the line as before.
    let usage_txt = match (threads, stats.usage) {
        (Some(t), Some(u)) => {
            let occ = occupancy(u.cpu_ms, u.wall_ms, t);
            series.occupancy.push(occ);
            format!(
                " cpu_ms {} wall_ms {:.0} occupancy {occ:.2}",
                u.cpu_ms, u.wall_ms
            )
        }
        _ => String::new(),
    };
    println!(
        "{name} [{scope}] {i}/{total}: {label} edits {n_edits} scope {}/{} check_ms {} revert_ms {} revert_ok {ok} new {new} rss_kb {rss_txt}{mode}{usage_txt}",
        stats.scope_files, stats.total_files, stats.check_ms, stats.revert_ms,
    );
    if i == 1
        && let Some(v) = verdict
    {
        for d in &v.new_errors.errors {
            println!("  + [{}] {}:{}", d.code, d.path, d.line);
        }
        for d in &v.new_errors.unmapped {
            println!("  + [ra:{}] {}:{}", d.ra_code, d.path, d.line);
        }
    }
    series.check_ms.push(stats.check_ms);
    series.revert_ms.push(stats.revert_ms);
    if let Some(k) = stats.rss_kb {
        series.rss_kb.push(k);
    }
}

/// Builds the analyzer of a bench on `backend`, loads `base` once and prints the load's lines to
/// stderr.
fn load(
    base: &std::path::Path,
    proc_macro: bool,
    assert_revert: bool,
    backend: Backend,
) -> Result<(RaAnalyzer, Arc<RecordingObserver>, ryo_v2_ra::RaSession)> {
    let observer = Arc::new(RecordingObserver::default());
    let analyzer = RaAnalyzer::new(
        RaConfig {
            proc_macro,
            assert_revert,
            ..RaConfig::default()
        }
        .with_backend(backend),
        Arc::new(MonotonicClock::new()),
        observer.clone(),
    );
    let session = analyzer.load(base)?;
    let rss_after_load = proc::vm_rss_kb().context("read the RSS after load")?;
    for event in observer.take() {
        match event {
            Event::Indexed { load_ms, files } => {
                eprintln!("[indexed] {load_ms} ms, {files} local files");
            }
            Event::Loaded {
                load_ms,
                baseline_ms,
                files,
                baseline_errors,
            } => {
                eprintln!("[load] {load_ms} ms, {files} local files");
                eprintln!(
                    "[baseline] {baseline_errors} pre-existing errors in {baseline_ms} ms (cold)"
                );
            }
            Event::Memory { .. } | Event::Checked { .. } | Event::Reverted { .. } => {}
        }
    }
    eprintln!("[rss] {rss_after_load} kB after load");
    Ok((analyzer, observer, session))
}

/// Loads `args.base` once and runs every variant; prints the fixed output format.
pub fn run(args: &BenchArgs) -> Result<()> {
    let (analyzer, observer, mut session) = load(
        &args.base,
        args.proc_macro,
        args.assert_revert,
        args.backend,
    )?;

    let parent = match &args.on {
        Some(dir) => {
            let edits = overlay_edits(&args.base, dir)
                .with_context(|| format!("overlay {}", dir.display()))?;
            if edits.is_empty() {
                anyhow::bail!("--on {}: no edits vs base", dir.display());
            }
            Some(edits)
        }
        None => None,
    };
    // With `--on` each run's usage is measured against the session's diagnostic threads.
    let threads = parent.as_ref().map(|_| session.threads());
    let modes: &[Mode] = if parent.is_some() {
        &[Mode::Rebuilt, Mode::Stacked]
    } else {
        &[Mode::Rebuilt]
    };

    let mut fail_infra = 0usize;
    // (median check_ms or first check_ms, label)
    let mut worst: Option<(u64, String)> = None;
    for variant in &args.variants {
        let edits = overlay_edits(&args.base, variant)
            .with_context(|| format!("overlay {}", variant.display()))?;
        let name = variant.display().to_string();
        if edits.is_empty() {
            println!("{name}: SKIP (no edits vs base)");
            continue;
        }
        // `child`: the edits of the child as the base sees them, the parent's with the variant's
        // put on top (rebuilt). `own`: those the child makes on top of the parent, a variant file
        // at the parent's text not being one (stacked).
        let (child, own) = match &parent {
            Some(parent) => {
                let own = stacked::own_edits(parent, &edits);
                if own.is_empty() {
                    println!("{name}: SKIP (no edits vs the --on parent)");
                    continue;
                }
                (
                    compose(parent, &edits, &|p| analyzer.text(&session, p)),
                    own,
                )
            }
            None => (edits, Vec::new()),
        };
        'scopes: for &scope in &args.scopes {
            let sname = scope_name(scope);
            for &mode in modes {
                let mode_txt = if parent.is_some() {
                    format!(" mode {}", mode.name())
                } else {
                    String::new()
                };
                let mut series = Series::default();
                let mut heading = Heading {
                    name: &name,
                    scope: sname,
                    mode: &mode_txt,
                    n_edits: child.len(),
                    repeat: args.repeat,
                    threads,
                };
                let (reverted, extra) = match (mode, &parent) {
                    (Mode::Stacked, Some(parent)) => {
                        heading.n_edits = own.len();
                        let mut rec = StackedRecord {
                            observer: &observer,
                            heading: &heading,
                            series: &mut series,
                            runs: 0,
                            rss_parent: None,
                            cpu: proc::cpu_ms,
                            started: None,
                        };
                        let reverted = stacked::stacked_series(
                            &analyzer,
                            &mut session,
                            &name,
                            parent,
                            &own,
                            scope,
                            args.repeat,
                            &mut |point| rec.observe(point),
                        )?;
                        // A series that returns has diagnosed the parent's level.
                        let rss = rec
                            .rss_parent
                            .context("the --on parent's level was not diagnosed")?;
                        (
                            reverted,
                            format!("{mode_txt} rss_kb_after_parent_level {rss}"),
                        )
                    }
                    _ => {
                        let reverted = repeat(&heading, &mut series, || {
                            rebuilt_once(
                                &analyzer,
                                &mut session,
                                &observer,
                                &child,
                                scope,
                                threads.is_some(),
                            )
                        })?;
                        (reverted, mode_txt.clone())
                    }
                };
                print_summary(
                    &name,
                    sname,
                    &mode_txt,
                    &extra,
                    args.backend,
                    &series,
                    &mut worst,
                );
                if !reverted {
                    fail_infra += 1;
                    break 'scopes;
                }
            }
        }
    }
    match worst {
        Some((x, label)) => println!(
            "core-4: {} (worst warm median check_ms {x} at {label}, limit 5000)",
            if x <= 5000 { "holds" } else { "exceeds" }
        ),
        None => println!("core-4: n/a (no runs)"),
    }
    if fail_infra > 0 {
        anyhow::bail!("{fail_infra} variant(s) failed revert assertion");
    }
    Ok(())
}

/// Arguments of one `bench --depth-sweep` invocation.
pub struct DepthArgs {
    /// The workspace to load.
    pub base: PathBuf,
    /// The depths to measure, in order: each is the number of levels of one stack.
    pub depths: Vec<u32>,
    /// The number of workspace files the levels rotate over.
    pub files: u32,
    /// Diagnoses of the top level per depth.
    pub repeat: u32,
    /// Scopes to diagnose, in order.
    pub scopes: Vec<Scope>,
    /// Whether to start the proc-macro server.
    pub proc_macro: bool,
    /// Whether every level's write is sampled for the re-assert at its pop.
    pub assert_revert: bool,
    /// The backend of the forks (`RaConfig.engine`); every summary line names it.
    pub backend: Backend,
}

/// What the hook of one depth series measured; durations in microseconds, by wall clock.
#[derive(Default)]
struct DepthRecord {
    /// The number of levels of the series.
    depth: usize,
    /// The number of diagnoses of the series.
    repeat: u32,
    /// The start of the port call in progress.
    started: Option<Instant>,
    write_us: Vec<u64>,
    check_us: Vec<u64>,
    pop_us: Vec<u64>,
    /// The scope files and findings of each diagnosis.
    diagnosed: Vec<(usize, usize)>,
    /// The VmRSS once every level is written.
    rss_after_stack: Option<u64>,
    /// The VmRSS after the last diagnosis, before the first pop.
    rss_after_diagnose: Option<u64>,
}

impl DepthRecord {
    fn elapsed_us(&mut self) -> u64 {
        self.started
            .take()
            .map_or(0, |t| t.elapsed().as_micros() as u64)
    }

    /// Times each write, diagnosis and pop between its points, and reads VmRSS with the strict
    /// reader once the stack is written and once it is diagnosed; a failed read ends the series.
    fn observe(&mut self, point: DepthPoint<'_>) -> Result<(), ProcError> {
        match point {
            DepthPoint::BeforeWrite(_)
            | DepthPoint::BeforeDiagnose(_)
            | DepthPoint::BeforePop(_) => self.started = Some(Instant::now()),
            DepthPoint::AfterWrite(k) => {
                let us = self.elapsed_us();
                self.write_us.push(us);
                if k == self.depth {
                    self.rss_after_stack = Some(proc::vm_rss_kb()?);
                }
            }
            DepthPoint::AfterDiagnose(r, found) => {
                let us = self.elapsed_us();
                self.check_us.push(us);
                self.diagnosed
                    .push((found.files.len(), found.findings.len()));
                if r == self.repeat {
                    self.rss_after_diagnose = Some(proc::vm_rss_kb()?);
                }
            }
            DepthPoint::AfterPop(_) => {
                let us = self.elapsed_us();
                self.pop_us.push(us);
            }
        }
        Ok(())
    }
}

/// Microseconds as milliseconds with three decimals.
fn ms(us: u64) -> String {
    format!("{}.{:03}", us / 1000, us % 1000)
}

/// (min/median/max) of microseconds in milliseconds, `-` when empty.
fn triple_ms(us: &[u64]) -> String {
    match summarize(us) {
        Some((a, b, c)) => format!("{}/{}/{}", ms(a), ms(b), ms(c)),
        None => "-".to_string(),
    }
}

/// The summary line of one depth: the first diagnosis of the top level, the warm ones (from the
/// second on) as min/median/max, the writes and pops per level as min/median/max and in total,
/// the VmRSS after the stack is written and after it is diagnosed, and the last diagnosis'
/// scope files and findings; ` backend <name>` ends it.
///
/// # Contract
/// - C1: one line per depth and scope, `summary depth-sweep [<scope>] depth <K> files <N>
///   n=<R>` followed by the figures in the order above, ending with ` backend <name>`; a figure
///   without a value (no warm diagnosis, no read) prints `-`.
///   tested: bench::tests::depth_summary_line_ends_with_the_backend
fn depth_summary_line(sname: &str, files: u32, rec: &DepthRecord, backend: Backend) -> String {
    let first = rec.check_us.first().map_or("-".to_string(), |&us| ms(us));
    let warm = rec.check_us.get(1..).unwrap_or(&[]);
    let opt = |x: Option<u64>| x.map_or("-".to_string(), |k| k.to_string());
    let (scope_files, findings) = rec
        .diagnosed
        .last()
        .map_or(("-".to_string(), "-".to_string()), |(s, f)| {
            (s.to_string(), f.to_string())
        });
    format!(
        "summary depth-sweep [{sname}] depth {} files {files} n={} first_check_ms {first} \
         warm_check_ms {} write_ms_per_level {} write_ms_total {} pop_ms_per_level {} \
         pop_ms_total {} rss_kb_after_stack {} rss_kb_after_diagnose {} scope_files \
         {scope_files} findings {findings} backend {}",
        rec.depth,
        rec.check_us.len(),
        triple_ms(warm),
        triple_ms(&rec.write_us),
        ms(rec.write_us.iter().sum()),
        triple_ms(&rec.pop_us),
        ms(rec.pop_us.iter().sum()),
        opt(rec.rss_after_stack),
        opt(rec.rss_after_diagnose),
        backend.name(),
    )
}

/// The `.rs` files under `dir`, relative to `root`, in path order; `target` and hidden dirs are
/// not walked.
fn rs_files(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<RelPath>) -> Result<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()
        .with_context(|| format!("read {}", dir.display()))?;
    entries.sort();
    for path in entries {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if path.is_dir() {
            if name != "target" && !name.starts_with('.') {
                rs_files(root, &path, out)?;
            }
        } else if name.ends_with(".rs")
            && let Ok(rel) = path.strip_prefix(root)
            && let Some(rel) = rel.to_str()
            && let Ok(rel) = RelPath::parse(&rel.replace(std::path::MAIN_SEPARATOR, "/"))
        {
            out.push(rel);
        }
    }
    Ok(())
}

/// Loads `args.base` once and, per scope and depth K, writes K levels on one fork with
/// `stacked::depth_series` (the levels of `stacked::depth_levels` over the first `args.files`
/// files of the workspace that the session indexed, in path order), diagnoses the top level
/// `args.repeat` times and pops back; prints a line per diagnosis and a summary per depth.
pub fn run_depth_sweep(args: &DepthArgs) -> Result<()> {
    let (analyzer, _observer, mut session) = load(
        &args.base,
        args.proc_macro,
        args.assert_revert,
        args.backend,
    )?;
    let mut paths = Vec::new();
    rs_files(&args.base, &args.base, &mut paths)?;
    let files: Vec<(RelPath, String)> = paths
        .into_iter()
        .filter_map(|p| analyzer.text(&session, &p).map(|t| (p, t)))
        .take(args.files as usize)
        .collect();
    if files.len() < args.files as usize {
        anyhow::bail!(
            "--depth-files {}: the workspace has {} indexed .rs files",
            args.files,
            files.len()
        );
    }
    let names: Vec<String> = files.iter().map(|(p, _)| p.to_string()).collect();
    eprintln!("[depth-sweep] levels rotate over {}", names.join(", "));
    let mut fail_infra = 0usize;
    'scopes: for &scope in &args.scopes {
        let sname = scope_name(scope);
        for &depth in &args.depths {
            let levels = stacked::depth_levels(&files, depth as usize);
            let mut rec = DepthRecord {
                depth: levels.len(),
                repeat: args.repeat,
                ..DepthRecord::default()
            };
            let name = format!("depth {depth}");
            let reverted = stacked::depth_series(
                &analyzer,
                &mut session,
                &name,
                &levels,
                scope,
                args.repeat,
                &mut |point| rec.observe(point),
            )?;
            for (i, (&us, &(scope_files, findings))) in
                rec.check_us.iter().zip(&rec.diagnosed).enumerate()
            {
                println!(
                    "depth {depth} [{sname}] {}/{}: check_ms {} scope_files {scope_files} findings {findings}",
                    i + 1,
                    args.repeat,
                    ms(us),
                );
            }
            println!(
                "{}",
                depth_summary_line(sname, args.files, &rec, args.backend)
            );
            if !reverted {
                fail_infra += 1;
                break 'scopes;
            }
        }
    }
    if fail_infra > 0 {
        anyhow::bail!("{fail_infra} depth series failed revert");
    }
    Ok(())
}

/// Prints the summary line of a series; `mode` is added to the label of the worst check.
fn print_summary(
    name: &str,
    sname: &str,
    mode: &str,
    extra: &str,
    backend: Backend,
    s: &Series,
    worst: &mut Option<(u64, String)>,
) {
    let Some(line) = summary_line(name, sname, extra, backend, s) else {
        return;
    };
    println!("{line}");
    // Without warm runs (n == 1) the first (cold) check stands in.
    let basis = summarize(&s.check_ms[1..]).map_or(s.check_ms[0], |(_, med, _)| med);
    if worst.as_ref().is_none_or(|(w, _)| basis > *w) {
        *worst = Some((basis, format!("{name} [{sname}]{mode}")));
    }
}

/// The summary line of a series, `None` for a series without runs: `extra` and the warm
/// occupancy follow the figures, and ` backend <name>` ends the line, so that runs on the two
/// backends can be told apart.
fn summary_line(
    name: &str,
    sname: &str,
    extra: &str,
    backend: Backend,
    s: &Series,
) -> Option<String> {
    let n = s.check_ms.len();
    if n == 0 {
        return None;
    }
    let warm_check = &s.check_ms[1..];
    let warm_revert = &s.revert_ms[1..];
    let warm_rss = if s.rss_kb.len() > 1 {
        &s.rss_kb[1..]
    } else {
        &[]
    };
    let rss_txt = if s.rss_kb.is_empty() {
        "-/-/-".to_string()
    } else {
        let max = s.rss_kb.iter().max().copied().unwrap_or(0);
        format!("{}/{}/{max}", s.rss_kb[0], s.rss_kb[s.rss_kb.len() - 1])
    };
    // Additive, with `--on`: the median occupancy of the warm runs (the first run alone when
    // there is no warm one).
    let warm_occupancy = if s.occupancy.len() > 1 {
        &s.occupancy[1..]
    } else {
        &s.occupancy[..]
    };
    let occ_txt = match median(warm_occupancy) {
        Some(m) => format!(" warm_occupancy_median {m:.2}"),
        None => String::new(),
    };
    Some(format!(
        "summary {name} [{sname}] n={n} first_check_ms {} warm_check_ms {} warm_revert_ms {} rss_kb {rss_txt} rss_slope_kb_per_iter {:.1} leak_suspect {}{extra}{occ_txt} backend {}",
        s.check_ms[0],
        triple(warm_check),
        triple(warm_revert),
        slope(warm_rss),
        leak_suspect(warm_rss),
        backend.name(),
    ))
}

/// (min, median, max); the median is the lower one for an even count (sorted[(n-1)/2]). None when empty.
pub fn summarize(xs: &[u64]) -> Option<(u64, u64, u64)> {
    if xs.is_empty() {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_unstable();
    Some((v[0], v[(v.len() - 1) / 2], v[v.len() - 1]))
}

/// The median of `xs`, the lower one for an even count, as [`summarize`] takes it. None when
/// empty.
pub fn median(xs: &[f64]) -> Option<f64> {
    let mut v = xs.to_vec();
    v.sort_unstable_by(f64::total_cmp);
    v.get(v.len().checked_sub(1)? / 2).copied()
}

/// Least-squares slope of xs against 0..n (per step); 0.0 when n < 2.
pub fn slope(xs: &[u64]) -> f64 {
    let n = xs.len();
    if n < 2 {
        return 0.0;
    }
    let nf = n as f64;
    let mean_x = (nf - 1.0) / 2.0;
    let mean_y = xs.iter().map(|&y| y as f64).sum::<f64>() / nf;
    let (mut num, mut den) = (0.0, 0.0);
    for (i, &y) in xs.iter().enumerate() {
        let dx = i as f64 - mean_x;
        num += dx * (y as f64 - mean_y);
        den += dx * dx;
    }
    num / den
}

/// warm = the series from the 2nd run on: n >= 5, slope > 0, and last - first > 5% of first.
pub fn leak_suspect(warm_rss_kb: &[u64]) -> bool {
    let n = warm_rss_kb.len();
    if n < 5 {
        return false;
    }
    let first = warm_rss_kb[0];
    let last = warm_rss_kb[n - 1];
    slope(warm_rss_kb) > 0.0 && last.saturating_sub(first) as f64 > first as f64 * 0.05
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_min_median_max() {
        assert_eq!(summarize(&[5, 1, 3]), Some((1, 3, 5)));
        assert_eq!(summarize(&[4, 1, 3, 2]), Some((1, 2, 4)));
        assert_eq!(summarize(&[]), None);
    }

    #[test]
    fn slope_of_linear_series() {
        assert!((slope(&[10, 20, 30, 40]) - 10.0).abs() < 1e-9);
        assert_eq!(slope(&[7, 7, 7]), 0.0);
        assert_eq!(slope(&[1]), 0.0);
    }

    #[test]
    fn leak_suspect_rule() {
        assert!(leak_suspect(&[100, 110, 120, 130, 140]));
        assert!(!leak_suspect(&[100, 101, 100, 101, 100]));
        assert!(!leak_suspect(&[100, 110, 120, 130]));
    }

    /// A CPU read that fails, as `proc::cpu_ms` does where `/proc/self/stat` is absent.
    fn failed_cpu() -> Result<u64, ProcError> {
        Err(ProcError::Read {
            file: PathBuf::from("/proc/self/stat"),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        })
    }

    #[test]
    fn occupancy_is_cpu_over_wall_times_threads() {
        // 400 CPU ms in 100 wall ms on 8 threads: half the cores.
        assert_eq!(occupancy(400, 100.0, 8), 0.5);
        assert_eq!(occupancy(100, 100.0, 1), 1.0);
        assert_eq!(occupancy(0, 50.0, 4), 0.0);

        // The bench's hook with threads: a repeat's line gets its usage, the series its
        // occupancy, and the figure is CPU over wall times threads.
        let observer = RecordingObserver::default();
        let heading = Heading {
            name: "fake",
            scope: "affected",
            mode: " mode stacked",
            n_edits: 1,
            repeat: 1,
            threads: Some(2),
        };
        let mut series = Series::default();
        let mut rec = StackedRecord {
            observer: &observer,
            heading: &heading,
            series: &mut series,
            runs: 0,
            rss_parent: None,
            cpu: proc::cpu_ms,
            started: None,
        };
        rec.observe(Point::BeforeWrite).unwrap();
        let mut x = 0u64;
        for _ in 0..2_000_000 {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        rec.observe(Point::AfterDiagnose).unwrap();
        rec.observe(Point::BeforePop).unwrap();
        rec.observe(Point::AfterPop(None)).unwrap();
        assert_eq!(series.occupancy.len(), 1);
        assert!(series.occupancy[0] >= 0.0 && series.occupancy[0].is_finite());

        // A failed CPU read at the start or the end of a repeat is the hook's error, so it ends
        // the series; no occupancy is recorded for that repeat.
        for fail_at_end in [false, true] {
            let mut series = Series::default();
            let mut rec = StackedRecord {
                observer: &observer,
                heading: &heading,
                series: &mut series,
                runs: 0,
                rss_parent: None,
                cpu: if fail_at_end {
                    proc::cpu_ms
                } else {
                    failed_cpu
                },
                started: None,
            };
            let begun = rec.observe(Point::BeforeWrite);
            let ended = if fail_at_end {
                begun.unwrap();
                rec.cpu = failed_cpu;
                rec.observe(Point::AfterPop(None))
            } else {
                begun
            };
            match ended {
                Err(e @ ProcError::Read { .. }) => {
                    assert!(e.to_string().contains("/proc/self/stat"), "{e}");
                }
                other => panic!("{other:?}"),
            }
            assert!(series.occupancy.is_empty());
        }
    }

    /// `ryo_v2_ra::Backend` C1: the word a summary line ends with.
    #[test]
    fn summary_line_ends_with_the_backend() {
        let series = Series {
            check_ms: vec![10, 20],
            revert_ms: vec![1, 2],
            rss_kb: Vec::new(),
            occupancy: Vec::new(),
        };
        // Without `--on` and `--engine`: the line of a bench before the backend was named, and
        // ` backend undo-log` after it.
        assert_eq!(
            summary_line("v", "affected", "", Backend::UndoLog, &series).unwrap(),
            "summary v [affected] n=2 first_check_ms 10 warm_check_ms 20/20/20 warm_revert_ms \
             2/2/2 rss_kb -/-/- rss_slope_kb_per_iter 0.0 leak_suspect false backend undo-log"
        );
        // With `--on`: the backend follows the mode, the parent's level and the occupancy.
        let stacked = Series {
            occupancy: vec![0.5, 0.25],
            ..series
        };
        assert_eq!(
            summary_line(
                "v",
                "workspace",
                " mode stacked rss_kb_after_parent_level 5",
                Backend::Engine,
                &stacked
            )
            .unwrap(),
            "summary v [workspace] n=2 first_check_ms 10 warm_check_ms 20/20/20 warm_revert_ms \
             2/2/2 rss_kb -/-/- rss_slope_kb_per_iter 0.0 leak_suspect false mode stacked \
             rss_kb_after_parent_level 5 warm_occupancy_median 0.25 backend engine"
        );
        assert!(summary_line("v", "affected", "", Backend::Engine, &Series::default()).is_none());
    }

    /// `depth_summary_line` C1.
    #[test]
    fn depth_summary_line_ends_with_the_backend() {
        let rec = DepthRecord {
            depth: 2,
            repeat: 3,
            write_us: vec![1500, 2500],
            check_us: vec![9000, 4000, 3000],
            pop_us: vec![250, 750],
            diagnosed: vec![(3, 0), (3, 0), (3, 1)],
            rss_after_stack: Some(100),
            rss_after_diagnose: Some(120),
            ..DepthRecord::default()
        };
        assert_eq!(
            depth_summary_line("affected", 1, &rec, Backend::Engine),
            "summary depth-sweep [affected] depth 2 files 1 n=3 first_check_ms 9.000 \
             warm_check_ms 3.000/3.000/4.000 write_ms_per_level 1.500/1.500/2.500 \
             write_ms_total 4.000 pop_ms_per_level 0.250/0.250/0.750 pop_ms_total 1.000 \
             rss_kb_after_stack 100 rss_kb_after_diagnose 120 scope_files 3 findings 1 \
             backend engine"
        );
        let empty = DepthRecord::default();
        let line = depth_summary_line("workspace", 2, &empty, Backend::UndoLog);
        assert!(line.contains("first_check_ms - warm_check_ms -"), "{line}");
        assert!(line.contains("rss_kb_after_stack - "), "{line}");
        assert!(line.ends_with(" backend undo-log"), "{line}");
    }

    /// The depth hook times between the points and reads VmRSS with the strict reader.
    #[test]
    fn depth_record_times_each_call_and_reads_rss_at_the_top() {
        let mut rec = DepthRecord {
            depth: 2,
            repeat: 1,
            ..DepthRecord::default()
        };
        let found = ryo_v2_core::domain::ScopedFindings::default();
        for point in [
            DepthPoint::BeforeWrite(1),
            DepthPoint::AfterWrite(1),
            DepthPoint::BeforeWrite(2),
            DepthPoint::AfterWrite(2),
            DepthPoint::BeforeDiagnose(1),
            DepthPoint::AfterDiagnose(1, &found),
            DepthPoint::BeforePop(2),
            DepthPoint::AfterPop(2),
            DepthPoint::BeforePop(1),
            DepthPoint::AfterPop(1),
        ] {
            rec.observe(point).unwrap();
        }
        assert_eq!(
            (rec.write_us.len(), rec.check_us.len(), rec.pop_us.len()),
            (2, 1, 2)
        );
        assert!(rec.rss_after_stack.is_some() && rec.rss_after_diagnose.is_some());
    }

    #[test]
    fn median_is_the_lower_middle() {
        assert_eq!(median(&[0.3, 0.1, 0.2]), Some(0.2));
        assert_eq!(median(&[0.4, 0.1, 0.3, 0.2]), Some(0.2));
        assert_eq!(median(&[]), None);
    }

    #[test]
    fn memory_event_round_trips_through_observer() {
        let o = RecordingObserver::default();
        o.observe(Event::Memory { rss_kb: 1 });
        let events = o.take();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Event::Memory { rss_kb: 1 }));
    }
}
