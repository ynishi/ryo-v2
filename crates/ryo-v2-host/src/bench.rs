//! The `bench` command body: load once, then repeat fork, apply, diagnose, discard.
//!
//! With `--on <parent overlay>` it measures one child two ways: `rebuilt`, the parent's edits
//! composed with the variant's and verified from the base, and `stacked`, the variant's edits
//! that the parent does not already make, written as a level on top of the parent's level of one
//! `Stacked` fork.

use crate::rss;
use anyhow::{Context, Result};
use ryo_v2_core::app::ports::{
    Analyzer, AnalyzerError, CheckError, Event, Observer, Stacked, Verifier,
};
use ryo_v2_core::app::verify::verify;
use ryo_v2_core::domain::search::compose;
use ryo_v2_core::domain::{Edit, Finding, RelPath, Scope, ScopedFindings, Verdict};
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
            Event::Loaded { .. } => {}
        }
    }
    stats
}

/// One `verify` of `edits` from the base.
fn rebuilt_once<A: Analyzer>(
    analyzer: &A,
    session: &mut A::Session,
    observer: &RecordingObserver,
    edits: &[Edit],
    scope: Scope,
) -> Result<Once> {
    observer.take();
    let result = verify(analyzer, session, edits, scope);
    sample_memory(observer);
    let stats = read_events(observer.take());
    let (label, verdict) = match result {
        Ok(v) => (if v.passed() { "PASS" } else { "FAIL" }, Some(v)),
        Err(AnalyzerError::RevertMismatch) => ("REVERT-MISMATCH", None),
        Err(e) => return Err(e.into()),
    };
    Ok(Once {
        stats,
        label,
        verdict,
    })
}

/// The diagnosis of the top level at `scope`, and the files a check of the edits that take the
/// base to its state covers (`Stacked::files_of`).
fn diagnose_top<A: Stacked>(
    a: &A,
    s: &A::Session,
    f: &A::Fork,
    scope: Scope,
) -> Result<(ScopedFindings, Vec<RelPath>), CheckError> {
    let found = a.diagnose(s, f, scope)?;
    Ok((found, a.files_of(s, f, scope)?))
}

/// One level of `edits` on top of the live fork's top level: write, diagnose, pop.
///
/// The verdict is formed as the search forms a child's: `Verdict::of_level` of the level's
/// diagnosis on top of `below`, the findings of the parent's level, kept to the files a check of
/// the composed edits covers. So it is the verdict the rebuilt mode gives for the same state.
#[allow(clippy::too_many_arguments)]
fn stacked_once<A: Stacked>(
    a: &A,
    s: &mut A::Session,
    f: &mut A::Fork,
    observer: &RecordingObserver,
    edits: &[Edit],
    scope: Scope,
    baseline: &[Finding],
    below: &ScopedFindings,
) -> Result<Once> {
    observer.take();
    // A refused write pushes no level, so there is nothing to pop.
    a.write(s, f, edits)
        .context("write the variant as a level")?;
    let diagnosed = diagnose_top(a, s, f, scope);
    let popped = a.pop(s, f);
    sample_memory(observer);
    let stats = read_events(observer.take());
    let (label, verdict) = match (diagnosed, popped) {
        (_, Err(CheckError::RevertMismatch { .. })) => ("REVERT-MISMATCH", None),
        (_, Err(e)) => return Err(e).context("pop the variant's level"),
        (Err(e), Ok(_)) => return Err(e).context("diagnose the variant's level"),
        (Ok((found, files)), Ok(_)) => {
            let (v, _) =
                Verdict::of_level(baseline, below, &found, &Edit::deleted_paths(edits), &files);
            (if v.passed() { "PASS" } else { "FAIL" }, Some(v))
        }
    };
    Ok(Once {
        stats,
        label,
        verdict,
    })
}

/// Opens a fork, writes `parent` as its level 1 and diagnoses it once; returns the RSS after that
/// level and the findings of its state, formed as the search forms its start state's. The fork is
/// the caller's to release.
fn parent_level<A: Stacked>(
    a: &A,
    s: &mut A::Session,
    f: &mut A::Fork,
    observer: &RecordingObserver,
    parent: &[Edit],
    scope: Scope,
    baseline: &[Finding],
) -> Result<(Option<u64>, ScopedFindings)> {
    a.write(s, f, parent)
        .context("write the --on parent as level 1")?;
    let (found, files) = diagnose_top(a, s, f, scope).context("diagnose the --on parent")?;
    let (_, kept) = Verdict::of_level(
        baseline,
        &ScopedFindings::default(),
        &found,
        &Edit::deleted_paths(parent),
        &files,
    );
    let rss_kb = rss::current_kb();
    observer.take();
    Ok((rss_kb, kept))
}

/// The repeats of one series on a fork of `a`: the parent is level 1, written and diagnosed once,
/// and each repeat writes `variant` as level 2, diagnoses it and pops it. The fork is released at
/// the end, whatever happened. Returns whether every run and the release of the parent's level
/// reverted, and the RSS after the parent level.
#[allow(clippy::too_many_arguments)]
fn stacked_series<A: Stacked>(
    a: &A,
    s: &mut A::Session,
    observer: &RecordingObserver,
    parent: &[Edit],
    variant: &[Edit],
    scope: Scope,
    heading: &Heading<'_>,
    series: &mut Series,
) -> Result<(bool, Option<u64>)> {
    let baseline = a.baseline(s);
    let mut f = a.fork(s).context("open the fork")?;
    let ran = parent_level(a, s, &mut f, observer, parent, scope, &baseline).and_then(
        |(rss_parent, below)| {
            let reverted = repeat(heading, series, || {
                stacked_once(a, s, &mut f, observer, variant, scope, &baseline, &below)
            })?;
            Ok((reverted, rss_parent))
        },
    );
    let released = a.release(s, f);
    let (reverted, rss_parent) = ran?;
    match released {
        Ok(()) => Ok((reverted, rss_parent)),
        // A failed revert poisoned the session, so its release fails too; the series is
        // counted as a revert failure, as a rebuilt one is, and the release failure is printed.
        Err(e) if !reverted => {
            eprintln!("{}: release after a failed revert: {e}", heading.name);
            Ok((false, rss_parent))
        }
        // The parent's level did not revert to the base: a revert failure of the series too.
        Err(e @ CheckError::RevertMismatch { .. }) => {
            eprintln!(
                "{}: REVERT-MISMATCH releasing the parent level: {e}",
                heading.name
            );
            Ok((false, rss_parent))
        }
        Err(e) => Err(e).context("release the fork"),
    }
}

/// Runs `heading.repeat` times, prints each run and adds it to `series`. Returns false at the
/// first run whose revert failed.
fn repeat(
    heading: &Heading<'_>,
    series: &mut Series,
    mut once: impl FnMut() -> Result<Once>,
) -> Result<bool> {
    let Heading {
        name,
        scope,
        mode,
        n_edits,
        repeat: total,
    } = *heading;
    for i in 1..=total {
        let Once {
            stats,
            label,
            verdict,
        } = once()?;
        let ok = match stats.revert_ok {
            Some(b) => b.to_string(),
            None => "-".to_string(),
        };
        let new = verdict.as_ref().map_or(0, |v| {
            v.new_errors.errors.len() + v.new_errors.unmapped.len()
        });
        let rss_txt = stats.rss_kb.map_or("-".to_string(), |k| k.to_string());
        println!(
            "{name} [{scope}] {i}/{total}: {label} edits {n_edits} scope {}/{} check_ms {} revert_ms {} revert_ok {ok} new {new} rss_kb {rss_txt}{mode}",
            stats.scope_files, stats.total_files, stats.check_ms, stats.revert_ms,
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
            return Ok(false);
        }
    }
    Ok(true)
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
                let own: Vec<Edit> = edits
                    .iter()
                    .filter(|e| !parent.contains(e))
                    .cloned()
                    .collect();
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
                };
                let (reverted, extra) = match (mode, &parent) {
                    (Mode::Stacked, Some(parent)) => {
                        heading.n_edits = own.len();
                        let (reverted, rss) = stacked_series(
                            &analyzer,
                            &mut session,
                            &observer,
                            parent,
                            &own,
                            scope,
                            &heading,
                            &mut series,
                        )?;
                        let rss = rss.map_or("-".to_string(), |k| k.to_string());
                        (
                            reverted,
                            format!("{mode_txt} rss_kb_after_parent_level {rss}"),
                        )
                    }
                    _ => {
                        let reverted = repeat(&heading, &mut series, || {
                            rebuilt_once(&analyzer, &mut session, &observer, &child, scope)
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
    println!(
        "summary {name} [{sname}] n={n} first_check_ms {} warm_check_ms {} warm_revert_ms {} rss_kb {rss_txt} rss_slope_kb_per_iter {:.1} leak_suspect {}{extra}",
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

    /// A fork whose every diagnosis is `top` and whose `files_of` is `files`.
    #[derive(Default)]
    struct FakeStack {
        levels: std::cell::RefCell<Vec<Vec<Edit>>>,
        top: ScopedFindings,
        files: Vec<RelPath>,
    }
    impl Verifier for FakeStack {
        type Session = ();
        fn text(&self, _: &(), _: &RelPath) -> Option<String> {
            None
        }
        fn facts(&self, _: &(), _: &RelPath) -> ryo_v2_core::domain::FileFacts {
            ryo_v2_core::domain::FileFacts::default()
        }
        fn baseline(&self, _: &()) -> Vec<Finding> {
            Vec::new()
        }
        fn provenance(&self, _: &()) -> ryo_v2_core::domain::Provenance {
            ryo_v2_core::domain::Provenance::new(
                "fake",
                "0",
                Default::default(),
                Default::default(),
            )
            .unwrap()
        }
        fn blind_classes(&self) -> std::collections::BTreeSet<ryo_v2_core::domain::UnseenClass> {
            Default::default()
        }
        fn check(&self, _: &mut (), _: &[Edit], _: Scope) -> Result<ScopedFindings, CheckError> {
            Ok(ScopedFindings::default())
        }
    }
    impl Stacked for FakeStack {
        type Fork = ();
        fn fork(&self, _: &mut ()) -> Result<(), CheckError> {
            Ok(())
        }
        fn write(&self, _: &mut (), _: &mut (), edits: &[Edit]) -> Result<(), CheckError> {
            self.levels.borrow_mut().push(edits.to_vec());
            Ok(())
        }
        fn levels(&self, _: &(), _: &()) -> Result<usize, CheckError> {
            Ok(self.levels.borrow().len())
        }
        fn fork_text(&self, _: &(), _: &(), _: &RelPath) -> Result<Option<String>, CheckError> {
            Ok(None)
        }
        fn diagnose(&self, _: &(), _: &(), _: Scope) -> Result<ScopedFindings, CheckError> {
            Ok(self.top.clone())
        }
        fn files_of(&self, _: &(), _: &(), _: Scope) -> Result<Vec<RelPath>, CheckError> {
            Ok(self.files.clone())
        }
        fn pop(&self, _: &mut (), _: &mut ()) -> Result<bool, CheckError> {
            Ok(self.levels.borrow_mut().pop().is_some())
        }
        fn release(&self, _: &mut (), _: ()) -> Result<(), CheckError> {
            self.levels.borrow_mut().clear();
            Ok(())
        }
    }

    #[test]
    fn a_stacked_verdict_is_formed_as_the_search_forms_it() {
        use ryo_v2_core::domain::UnseenClass;
        use std::collections::{BTreeMap, BTreeSet};
        let rel = |p: &str| RelPath::parse(p).unwrap();
        // The parent's level observed a class on b; the variant's level diagnoses a only.
        let below = ScopedFindings {
            files: vec![rel("a.rs"), rel("b.rs")],
            unseen_at: BTreeMap::from([(
                UnseenClass::UnresolvedPath,
                BTreeSet::from([rel("b.rs")]),
            )]),
            ..ScopedFindings::default()
        };
        let a = FakeStack {
            top: ScopedFindings {
                files: vec![rel("a.rs")],
                ..ScopedFindings::default()
            },
            files: vec![rel("a.rs"), rel("b.rs")],
            ..FakeStack::default()
        };
        let observer = RecordingObserver::default();
        let variant = [Edit::write(rel("a.rs"), "fn a() {}")];
        let once = stacked_once(
            &a,
            &mut (),
            &mut (),
            &observer,
            &variant,
            Scope::Affected,
            &[],
            &below,
        )
        .unwrap();
        let want = Verdict::of_level(&[], &below, &a.top, &[], &a.files).0;
        assert_eq!(once.verdict, Some(want));
        // The class kept from below is named: the top level's diagnosis alone would not name it.
        let named = once.verdict.map(|v| v.unseen).unwrap_or_default();
        assert_eq!(named, BTreeSet::from([UnseenClass::UnresolvedPath]));
        assert!(Verdict::from_baseline(&[], &a.top).unseen.is_empty());
        assert_eq!(once.label, "PASS");
        assert!(a.levels.borrow().is_empty(), "the level is popped");
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
