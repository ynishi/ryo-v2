//! The `bench` command body: load once, then repeat fork, apply, diagnose, discard.

use crate::rss;
use anyhow::{Context, Result};
use ryo_v2_core::app::ports::{Analyzer, AnalyzerError, Event, Observer};
use ryo_v2_core::app::verify::verify;
use ryo_v2_core::domain::Scope;
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_core::infra::overlay::overlay_edits;
use ryo_v2_ra::{RaAnalyzer, RaConfig};
use std::path::PathBuf;
use std::sync::Arc;

/// Arguments of one bench invocation.
pub struct BenchArgs {
    /// The workspace to load.
    pub base: PathBuf,
    /// Overlay dirs; each .rs that differs from (or is missing in) `base` is an edit.
    pub variants: Vec<PathBuf>,
    /// Runs per (variant, scope).
    pub repeat: u32,
    /// Scopes to diagnose, in order.
    pub scopes: Vec<Scope>,
    /// Whether to start the proc-macro server.
    pub proc_macro: bool,
    /// Whether to re-diagnose after the revert and compare with the baseline.
    pub assert_revert: bool,
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
}

/// The measured series of one (variant, scope).
#[derive(Default)]
struct Series {
    check_ms: Vec<u64>,
    revert_ms: Vec<u64>,
    rss_kb: Vec<u64>,
}

fn scope_name(scope: Scope) -> &'static str {
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

fn sample_memory(observer: &RecordingObserver) {
    if let Some(rss_kb) = rss::current_kb() {
        observer.observe(Event::Memory { rss_kb });
    }
}

/// Loads `args.base` once and runs every variant; prints the fixed output format.
pub fn run(args: &BenchArgs) -> Result<()> {
    let observer = Arc::new(RecordingObserver::default());
    let analyzer = RaAnalyzer::new(
        RaConfig {
            proc_macro: args.proc_macro,
            threads: None,
            assert_revert: args.assert_revert,
            gap_detectors: true,
            assert_revert_every: None,
            cfg_test: false,
        },
        Arc::new(MonotonicClock::new()),
        observer.clone(),
    );
    let mut session = analyzer.load(&args.base)?;
    sample_memory(&observer);
    let mut rss_after_load = None;
    for event in observer.take() {
        match event {
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
            Event::Memory { rss_kb } => rss_after_load = Some(rss_kb),
            Event::Checked { .. } | Event::Reverted { .. } => {}
        }
    }
    match rss_after_load {
        Some(kb) => eprintln!("[rss] {kb} kB after load"),
        None => eprintln!("[rss] - kB after load"),
    }

    let mut fail_infra = 0usize;
    // (median check_ms or first check_ms, label)
    let mut worst: Option<(u64, String)> = None;
    for variant in &args.variants {
        let edits = overlay_edits(&args.base, variant)
            .with_context(|| format!("overlay {}", variant.display()))?;
        let name = variant.display();
        if edits.is_empty() {
            println!("{name}: SKIP (no edits vs base)");
            continue;
        }
        let n_edits = edits.len();
        'scopes: for &scope in &args.scopes {
            let sname = scope_name(scope);
            let mut series = Series::default();
            for i in 1..=args.repeat {
                observer.take();
                let result = verify(&analyzer, &mut session, &edits, scope);
                sample_memory(&observer);
                let mut stats = RunStats::default();
                for event in observer.take() {
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
                        Event::Loaded { .. } => {}
                    }
                }
                let (label, verdict) = match result {
                    Ok(v) => (if v.passed() { "PASS" } else { "FAIL" }, Some(v)),
                    Err(AnalyzerError::RevertMismatch) => ("REVERT-MISMATCH", None),
                    Err(e) => return Err(e.into()),
                };
                let ok = match stats.revert_ok {
                    Some(b) => b.to_string(),
                    None => "-".to_string(),
                };
                let new = verdict.as_ref().map_or(0, |v| {
                    v.new_errors.errors.len() + v.new_errors.unmapped.len()
                });
                let rss_txt = stats.rss_kb.map_or("-".to_string(), |k| k.to_string());
                println!(
                    "{name} [{sname}] {i}/{n}: {label} edits {n_edits} scope {}/{} check_ms {} revert_ms {} revert_ok {ok} new {new} rss_kb {rss_txt}",
                    stats.scope_files,
                    stats.total_files,
                    stats.check_ms,
                    stats.revert_ms,
                    n = args.repeat,
                );
                if i == 1
                    && let Some(v) = &verdict
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
                if verdict.is_none() {
                    fail_infra += 1;
                    print_summary(&name.to_string(), sname, &series, &mut worst);
                    break 'scopes;
                }
            }
            print_summary(&name.to_string(), sname, &series, &mut worst);
        }
    }
    match worst {
        Some((x, label)) => println!(
            "principle-15: {} (worst warm median check_ms {x} at {label}, limit 5000)",
            if x <= 5000 { "holds" } else { "exceeds" }
        ),
        None => println!("principle-15: n/a (no runs)"),
    }
    if fail_infra > 0 {
        anyhow::bail!("{fail_infra} variant(s) failed revert assertion");
    }
    Ok(())
}

fn print_summary(name: &str, sname: &str, s: &Series, worst: &mut Option<(u64, String)>) {
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
    println!(
        "summary {name} [{sname}] n={n} first_check_ms {} warm_check_ms {} warm_revert_ms {} rss_kb {rss_txt} rss_slope_kb_per_iter {:.1} leak_suspect {}",
        s.check_ms[0],
        triple(warm_check),
        triple(warm_revert),
        slope(warm_rss),
        leak_suspect(warm_rss),
    );
    // Without warm runs (n == 1) the first (cold) check stands in.
    let basis = summarize(warm_check).map_or(s.check_ms[0], |(_, med, _)| med);
    if worst.as_ref().is_none_or(|(w, _)| basis > *w) {
        *worst = Some((basis, format!("{name} [{sname}]")));
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

    #[test]
    fn memory_event_round_trips_through_observer() {
        let o = RecordingObserver::default();
        o.observe(Event::Memory { rss_kb: 1 });
        let events = o.take();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Event::Memory { rss_kb: 1 }));
    }
}
