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

use crate::proc::{self, ProcError};
use crate::stacked::{self, Point};
use anyhow::{Context, Result};
use ryo_v2_core::app::ports::{Analyzer, AnalyzerError, Event, Observer, Verifier};
use ryo_v2_core::app::verify::verify;
use ryo_v2_core::domain::search::compose;
use ryo_v2_core::domain::{Edit, Scope, Verdict};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_core::infra::overlay::overlay_edits;
use ryo_v2_ra::{RaAnalyzer, RaConfig};
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

/// Loads `args.base` once and runs every variant; prints the fixed output format.
pub fn run(args: &BenchArgs) -> Result<()> {
    let observer = Arc::new(RecordingObserver::default());
    let analyzer = RaAnalyzer::new(
        RaConfig {
            proc_macro: args.proc_macro,
            assert_revert: args.assert_revert,
            ..RaConfig::default()
        },
        Arc::new(MonotonicClock::new()),
        observer.clone(),
    );
    let mut session = analyzer.load(&args.base)?;
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
                print_summary(&name, sname, &mode_txt, &extra, &series, &mut worst);
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

/// Prints the summary line of a series; `mode` is added to the label of the worst check, `extra`
/// ends the line.
fn print_summary(
    name: &str,
    sname: &str,
    mode: &str,
    extra: &str,
    s: &Series,
    worst: &mut Option<(u64, String)>,
) {
    let n = s.check_ms.len();
    if n == 0 {
        return;
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
    println!(
        "summary {name} [{sname}] n={n} first_check_ms {} warm_check_ms {} warm_revert_ms {} rss_kb {rss_txt} rss_slope_kb_per_iter {:.1} leak_suspect {}{extra}{occ_txt}",
        s.check_ms[0],
        triple(warm_check),
        triple(warm_revert),
        slope(warm_rss),
        leak_suspect(warm_rss),
    );
    // Without warm runs (n == 1) the first (cold) check stands in.
    let basis = summarize(warm_check).map_or(s.check_ms[0], |(_, med, _)| med);
    if worst.as_ref().is_none_or(|(w, _)| basis > *w) {
        *worst = Some((basis, format!("{name} [{sname}]{mode}")));
    }
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
