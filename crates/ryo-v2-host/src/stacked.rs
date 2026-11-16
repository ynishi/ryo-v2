//! The one stacked procedure of the host, on `Forks` at W = 1: a parent's level is a job on the
//! base's root, and per repeat a variant's level is a job on the parent's Layer, diagnosed and
//! dropped (D11 stage 4).
//!
//! Every binary of the host that measures the stacked mode runs this function and observes it
//! through its hook, so that what one measures is exactly the procedure another times.
//!
//! A level is one job of [`Forks::submit`] on the Layer of the level below; the series keeps one
//! job in flight at a time (W = 1) and waits for its outcome before it goes on. The job writes its
//! edits and diagnoses its scope on a worker, so a level's write and its diagnosis are one step:
//! the hook's write point is the job's submit (its edits resolved and queued). A pop drops the
//! level's Layer; nothing is reverted, and the base's isolation (I2) is checked once, at the
//! `close` that ends every series.

use crate::proc::ProcError;
use anyhow::{Context, Result};
use ryo_v2_core::app::ports::{CheckError, Checked, Forks, JobId, Limit, Next};
use ryo_v2_core::domain::{Edit, Finding, RelPath, Scope, ScopedFindings, Verdict};
use std::time::Duration;

/// How long one wait in `Forks::next` lasts before the series asks again.
const TICK: Duration = Duration::from_millis(100);

/// A point of [`stacked_series`] at which its hook is called.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Point<'a> {
    /// The parent's level is written and diagnosed (its job ran); once per series.
    ParentDiagnosed,
    /// A repeat is about to submit the variant's level.
    BeforeWrite,
    /// The variant's level is submitted (its edits resolved and queued); its write and its
    /// diagnosis run together on a worker, which may already have started them.
    AfterWrite,
    /// The variant's level has been diagnosed (or its job failed).
    AfterDiagnose,
    /// The variant's level is about to be dropped.
    BeforePop,
    /// The variant's level has been dropped: the level's verdict.
    AfterPop(&'a Verdict),
}

impl Point<'_> {
    /// The point's name: the variant's own name, as the series' errors and the test logs print it.
    pub fn name(self) -> &'static str {
        match self {
            Point::ParentDiagnosed => "ParentDiagnosed",
            Point::BeforeWrite => "BeforeWrite",
            Point::AfterWrite => "AfterWrite",
            Point::AfterDiagnose => "AfterDiagnose",
            Point::BeforePop => "BeforePop",
            Point::AfterPop(_) => "AfterPop",
        }
    }
}

/// The hook of [`stacked_series`]: it observes a point and may end the series with a failed read.
pub type Hook<'h> = dyn FnMut(Point<'_>) -> Result<(), ProcError> + 'h;

/// The edits of `edits` that `parent` does not already make: what a variant writes as a level on
/// top of the parent's level (a variant file at the parent's text is not one).
pub fn own_edits(parent: &[Edit], edits: &[Edit]) -> Vec<Edit> {
    edits
        .iter()
        .filter(|e| !parent.contains(e))
        .cloned()
        .collect()
}

/// Submits `edits` as a job on `parent` at `scope`, under the adapter's check cut-off (D1).
fn submit<A: Forks>(
    a: &A,
    s: &mut A::Session,
    parent: &A::Layer,
    edits: &[Edit],
    scope: Scope,
) -> Result<JobId, CheckError> {
    a.submit(s, parent, edits, scope, Limit::Check)
}

/// Waits for the outcome of `id`, the one job the series has in flight: `Ok` with the job's own
/// result (an error of reach Blueprint inside it), or the error of the wait (the session's, or an
/// outcome that is not that job's).
pub(crate) fn outcome<A: Forks>(
    a: &A,
    s: &mut A::Session,
    id: JobId,
) -> Result<Result<Checked<A::Layer>, CheckError>> {
    loop {
        match a.next(s, TICK).context("wait for the job")? {
            Next::Done(got, result) if got == id => return Ok(result),
            Next::Withdrawn(got) if got == id => anyhow::bail!("the job was withdrawn"),
            Next::Pending => {}
            Next::Idle => anyhow::bail!("the job gave no outcome"),
            Next::Done(..) | Next::Expanded(..) | Next::Withdrawn(_) => {
                anyhow::bail!("an outcome of a job the series did not submit")
            }
        }
    }
}

/// The error of a level's job that failed: a refused edit names the write, any other failure the
/// diagnosis.
fn job_error(e: CheckError, write: String, diagnose: String) -> anyhow::Error {
    let what = if matches!(e, CheckError::Edit { .. }) {
        write
    } else {
        diagnose
    };
    anyhow::Error::from(e).context(what)
}

/// The repeats of one series on forks of `a`: `parent` is level 1, a job on the base's root,
/// written and diagnosed once, and each of `repeat` repeats submits `variant` as level 2 on the
/// parent's Layer, waits for its diagnosis and drops its Layer. The forks are closed at the end,
/// whatever happened. Returns whether the base's isolation held at that close (I2).
///
/// A level's verdict is formed as the search forms a child's: `Verdict::of_level` of the level's
/// diagnosis on top of the findings of the parent's level (themselves formed as the search forms
/// its start state's), kept to the files a check of the composed edits covers
/// (`Forks::layer_files`). So it is the verdict a rebuilt check gives for the same state. `name`
/// labels what is printed to stderr.
///
/// # Contract
/// - C1: the one procedure of the stacked mode for every binary of the host: it opens the forks,
///   submits the parent as a job on the root, waits for it and calls the hook at
///   ParentDiagnosed; per repeat it calls the hook at BeforeWrite, submits the variant as a job
///   on the parent's Layer, calls it at AfterWrite, waits for the job's diagnosis, calls it at
///   AfterDiagnose and BeforePop, drops the job's Layer and calls it at AfterPop, in that order;
///   then it drops every Layer and closes the forks. One job is in flight at a time (W = 1). The
///   hook observes and never changes the session.
///   tested: stacked::tests::probe_and_bench_run_the_same_order
///   tested: stacked::tests::a_stacked_verdict_is_formed_as_the_search_forms_it
/// - C2: a hook error (a failed read of `/proc`) ends the series with that error; the forks are
///   still closed after every Layer is dropped, and a failed close is printed, the series' error
///   being the one returned. A failure of the repeat that the hook's error cuts short (a failed
///   diagnosis of the variant's level) is named in that error. A close that finds the base
///   changed (`Poisoned`) makes the series return `Ok(false)`.
///   tested: stacked::tests::a_failed_hook_read_ends_the_series
///   tested: stacked::tests::a_failed_close_keeps_the_series_error
///   tested: stacked::tests::a_hook_error_names_the_failure_it_cuts_short
///   tested: stacked::tests::a_poisoned_close_is_an_isolation_failure
#[allow(clippy::too_many_arguments)]
pub fn stacked_series<A: Forks>(
    a: &A,
    s: &mut A::Session,
    name: &str,
    parent: &[Edit],
    variant: &[Edit],
    scope: Scope,
    repeat: u32,
    hook: &mut Hook<'_>,
) -> Result<bool> {
    let baseline = a.baseline(s);
    let root = a.open(s).context("open the forks")?;
    let ran = levels(a, s, &root, parent, variant, scope, repeat, &baseline, hook);
    drop(root);
    let closed = a.close(s);
    settle(name, ran, closed)
}

/// The outcome of a series from what its levels returned (`ran`, every Layer of theirs dropped)
/// and what the close of its forks returned: the series' error wins over the close's, which is
/// printed; a close that found the base changed (`Poisoned`, I2) counts as an isolation failure
/// of the series, as a revert that did not restore the base counted before.
fn settle(name: &str, ran: Result<()>, closed: Result<(), CheckError>) -> Result<bool> {
    if let Err(e) = ran {
        // The series' error is the one returned; the close's is printed, not lost.
        if let Err(c) = closed {
            eprintln!("{name}: close after a failed series: {c}");
        }
        return Err(e);
    }
    match closed {
        Ok(()) => Ok(true),
        Err(e @ CheckError::Poisoned(_)) => {
            eprintln!("{name}: ISOLATION-FAILED closing the forks (I2): {e}");
            Ok(false)
        }
        Err(e) => Err(e).context("close the forks"),
    }
}

/// The levels of [`stacked_series`] on the open forks, from `root`; every Layer they hold is
/// dropped when they return, and the forks are the caller's to close.
#[allow(clippy::too_many_arguments)]
fn levels<A: Forks>(
    a: &A,
    s: &mut A::Session,
    root: &A::Layer,
    parent: &[Edit],
    variant: &[Edit],
    scope: Scope,
    repeat: u32,
    baseline: &[Finding],
    hook: &mut Hook<'_>,
) -> Result<()> {
    let id = submit(a, s, root, parent, scope).context("submit the --on parent as level 1")?;
    let level1 = outcome(a, s, id)?.map_err(|e| {
        job_error(
            e,
            "write the --on parent as level 1".to_owned(),
            "diagnose the --on parent".to_owned(),
        )
    })?;
    let files = a
        .layer_files(s, &level1.layer, scope)
        .context("the files of the --on parent")?;
    let (_, below) = Verdict::of_level(
        baseline,
        &ScopedFindings::default(),
        &level1.findings,
        &Edit::deleted_paths(parent),
        &files,
    );
    call(hook, Point::ParentDiagnosed, None)?;
    let deleted = Edit::deleted_paths(variant);
    for _ in 0..repeat {
        call(hook, Point::BeforeWrite, None)?;
        let id =
            submit(a, s, &level1.layer, variant, scope).context("submit the variant as a level")?;
        // A hook error here leaves the job in flight; the close withdraws it.
        call(hook, Point::AfterWrite, None)?;
        let diagnosed = outcome(a, s, id)?.and_then(|checked| {
            let files = a.layer_files(s, &checked.layer, scope)?;
            Ok((checked, files))
        });
        call(hook, Point::AfterDiagnose, diagnosed.as_ref().err())?;
        call(hook, Point::BeforePop, diagnosed.as_ref().err())?;
        let (checked, files) = diagnosed.map_err(|e| {
            job_error(
                e,
                "write the variant as a level".to_owned(),
                "diagnose the variant's level".to_owned(),
            )
        })?;
        let verdict = Verdict::of_level(baseline, &below, &checked.findings, &deleted, &files).0;
        // The pop: the level's Layer is dropped.
        drop(checked);
        call(hook, Point::AfterPop(&verdict), None)?;
    }
    Ok(())
}

/// Calls `hook` at `point`. Its error ends the series; `pending`, a failure the series met before
/// that point and had not yet reported (a failed diagnosis), is named in that error, so that
/// ending the series loses neither.
fn call(hook: &mut Hook<'_>, point: Point<'_>, pending: Option<&CheckError>) -> Result<()> {
    let at = point.name();
    hook(point).map_err(|e| {
        let e = anyhow::Error::from(e);
        match pending {
            Some(p) => e.context(format!("the hook at {at}, after the series met: {p}")),
            None => e.context(format!("the hook at {at}")),
        }
    })
}

/// A point of [`depth_series`] at which its hook is called. Levels and diagnoses count from 1.
#[derive(Clone, Copy, Debug)]
pub enum DepthPoint<'a> {
    /// Level `k` is about to be submitted.
    BeforeWrite(usize),
    /// Level `k` is written: submitted, and for a level the shape does not diagnose, its job
    /// done (its write and its diagnosis are one job).
    AfterWrite(usize),
    /// Diagnosis `r` of the top level is about to run (its job is submitted or about to be).
    BeforeDiagnose(u32),
    /// Diagnosis `r` of the top level has run, with its findings.
    AfterDiagnose(u32, &'a ScopedFindings),
    /// Level `k`, the top one, is about to be dropped.
    BeforePop(usize),
    /// Level `k`'s Layer has been dropped.
    AfterPop(usize),
}

impl DepthPoint<'_> {
    /// The point's name, for the error of a hook that fails at it.
    pub fn name(self) -> &'static str {
        match self {
            DepthPoint::BeforeWrite(_) => "BeforeWrite",
            DepthPoint::AfterWrite(_) => "AfterWrite",
            DepthPoint::BeforeDiagnose(_) => "BeforeDiagnose",
            DepthPoint::AfterDiagnose(..) => "AfterDiagnose",
            DepthPoint::BeforePop(_) => "BeforePop",
            DepthPoint::AfterPop(_) => "AfterPop",
        }
    }
}

/// The hook of [`depth_series`]: it observes a point and may end the series with a failed read.
pub type DepthHook<'h> = dyn FnMut(DepthPoint<'_>) -> Result<(), ProcError> + 'h;

/// What [`depth_series`] calls once every level is written and diagnosed: the session and the
/// levels' Layers, the lowest first.
pub type OnStacked<'h, S, L> = dyn FnMut(&S, &[L]) + 'h;

/// The levels of a depth series of `depth` levels over `files` (each a path and its base text).
/// Level k appends `fn __ryo_depth_<k>() -> u32 { <k> }` to file `(k - 1) % files.len()` and is
/// written as that file's composed text so far, so that every level is one edit of one input.
/// No `files`, no level.
///
/// # Contract
/// - C1: level k is one write of file `(k - 1) % files.len()` whose text is that file's base text
///   with the functions of every level up to k on that file appended in order; a level never
///   repeats the text of the level below.
///   tested: stacked::tests::depth_levels_compose_one_edit_per_level
pub fn depth_levels(files: &[(RelPath, String)], depth: usize) -> Vec<Vec<Edit>> {
    if files.is_empty() {
        return Vec::new();
    }
    let mut texts: Vec<String> = files.iter().map(|(_, t)| t.clone()).collect();
    (1..=depth)
        .map(|k| {
            let i = (k - 1) % files.len();
            let text = &mut texts[i];
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&format!("\nfn __ryo_depth_{k}() -> u32 {{\n    {k}\n}}\n"));
            vec![Edit::write(files[i].0.clone(), text.clone())]
        })
        .collect()
}

/// Which levels of a [`depth_series`] are diagnosed, as the hook sees it. On `Forks` every level
/// is a job, and a job diagnoses its scope; the shape says which of those diagnoses the hook is
/// shown, and how often the top level is diagnosed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepthShape {
    /// The top level, this many times, once every level below is written (`bench
    /// --depth-sweep`): diagnosis 1 is the top level's own job, and diagnosis r ≥ 2 the same
    /// level run again as a job on the level below, its Layer dropped. The levels below are jobs
    /// awaited within their write points.
    Top(u32),
    /// Every level once, right after its submit and before the next level is submitted on it
    /// (the search's shape, `engine-probe --depth`): each level below the top holds its own
    /// diagnosis.
    EveryLevel,
}

/// One depth series on forks of `a`: submits each of `levels` as a job on the Layer of the one
/// before (the first on the base's root), waits for each, calls the hook at the diagnoses `shape`
/// says, calls `on_stacked` with the session and the levels' Layers (the lowest first) once every
/// level is written and diagnosed, then drops the Layers from the top. The forks are closed at the
/// end, whatever happened. Returns whether the base's isolation held at that close (I2). `name`
/// labels what is printed to stderr.
///
/// It shares the port calls of [`stacked_series`] (`open`, `submit`, `next`, `close`) and its
/// handling of a failed close; only the shape of the stack differs.
///
/// # Contract
/// - C1: it opens the forks; per level k it calls the hook at BeforeWrite(k), submits the level
///   on the Layer of level k − 1 (the root for k = 1) and calls it at AfterWrite(k); a level the
///   shape does not diagnose is awaited before AfterWrite(k). With `DepthShape::EveryLevel` the
///   wait for level k is diagnosis k, between BeforeDiagnose(k) and AfterDiagnose(k), right
///   after AfterWrite(k); with `DepthShape::Top(R)` the wait for the top level K is diagnosis 1,
///   and each diagnosis r of 2..=R is the top level's edits submitted again on level K − 1's
///   Layer and awaited between BeforeDiagnose(r) and AfterDiagnose(r), its Layer dropped. Then it
///   calls `on_stacked` once with the K Layers; per level k from the top it calls the hook at
///   BeforePop(k), drops the Layer and calls it at AfterPop(k); then it closes the forks. So K
///   levels are written in order and K are dropped in the reverse order, one job in flight at a
///   time (W = 1). The hook and `on_stacked` observe and never change the session.
///   tested: stacked::tests::a_depth_series_writes_k_levels_and_pops_k_in_order
///   tested: stacked::tests::an_every_level_series_diagnoses_each_level_before_the_next
/// - C2: a hook error, a refused write or a failed diagnosis ends the series with that error,
///   `on_stacked` is not called, and the forks are still closed after every Layer is dropped; a
///   close that finds the base changed (`Poisoned`, I2) makes the series return `Ok(false)`.
///   tested: stacked::tests::a_depth_series_stops_at_a_failure_and_closes
#[allow(clippy::too_many_arguments)]
pub fn depth_series<A: Forks>(
    a: &A,
    s: &mut A::Session,
    name: &str,
    levels: &[Vec<Edit>],
    scope: Scope,
    shape: DepthShape,
    hook: &mut DepthHook<'_>,
    on_stacked: &mut OnStacked<'_, A::Session, A::Layer>,
) -> Result<bool> {
    let root = a.open(s).context("open the forks")?;
    let ran = depth_levels_on(a, s, &root, levels, scope, shape, hook, on_stacked);
    drop(root);
    let closed = a.close(s);
    settle(name, ran, closed)
}

/// The levels of [`depth_series`] on the open forks, from `root`; every Layer they hold is
/// dropped when they return, and the forks are the caller's to close.
#[allow(clippy::too_many_arguments)]
fn depth_levels_on<A: Forks>(
    a: &A,
    s: &mut A::Session,
    root: &A::Layer,
    levels: &[Vec<Edit>],
    scope: Scope,
    shape: DepthShape,
    hook: &mut DepthHook<'_>,
    on_stacked: &mut OnStacked<'_, A::Session, A::Layer>,
) -> Result<()> {
    let depth = levels.len();
    // chain[k - 1] is level k's Layer.
    let mut chain: Vec<A::Layer> = Vec::with_capacity(depth);
    for (i, edits) in levels.iter().enumerate() {
        let k = i + 1;
        // Which diagnosis of the shape level k's job is, if the hook is shown it: (r, of).
        let shown = match shape {
            DepthShape::EveryLevel => Some((k as u32, depth as u32)),
            DepthShape::Top(repeat) if k == depth && repeat > 0 => Some((1, repeat)),
            DepthShape::Top(_) => None,
        };
        depth_call(hook, DepthPoint::BeforeWrite(k))?;
        let parent = chain.last().unwrap_or(root);
        let id = submit(a, s, parent, edits, scope).with_context(|| format!("submit level {k}"))?;
        let checked = match shown {
            None => {
                let checked = outcome(a, s, id)?.map_err(|e| {
                    job_error(e, format!("write level {k}"), format!("diagnose level {k}"))
                })?;
                depth_call(hook, DepthPoint::AfterWrite(k))?;
                checked
            }
            Some((r, of)) => {
                depth_call(hook, DepthPoint::AfterWrite(k))?;
                depth_call(hook, DepthPoint::BeforeDiagnose(r))?;
                let checked = outcome(a, s, id)?.map_err(|e| {
                    job_error(
                        e,
                        format!("write level {k}"),
                        format!("diagnose the top level ({r}/{of})"),
                    )
                })?;
                depth_call(hook, DepthPoint::AfterDiagnose(r, &checked.findings))?;
                checked
            }
        };
        chain.push(checked.layer);
    }
    if let (DepthShape::Top(repeat), Some(top)) = (shape, levels.last()) {
        for r in 2..=repeat {
            depth_call(hook, DepthPoint::BeforeDiagnose(r))?;
            let below = depth
                .checked_sub(2)
                .and_then(|i| chain.get(i))
                .unwrap_or(root);
            let id = submit(a, s, below, top, scope)
                .with_context(|| format!("submit the top level again ({r}/{repeat})"))?;
            let checked = outcome(a, s, id)?.map_err(|e| {
                job_error(
                    e,
                    format!("write the top level again ({r}/{repeat})"),
                    format!("diagnose the top level ({r}/{repeat})"),
                )
            })?;
            depth_call(hook, DepthPoint::AfterDiagnose(r, &checked.findings))?;
            // A sibling of the top level: its Layer is dropped here.
        }
    }
    on_stacked(s, &chain);
    for k in (1..=depth).rev() {
        depth_call(hook, DepthPoint::BeforePop(k))?;
        chain.pop();
        depth_call(hook, DepthPoint::AfterPop(k))?;
    }
    Ok(())
}

/// Calls `hook` at `point`; its error ends the series, named with the point.
fn depth_call(hook: &mut DepthHook<'_>, point: DepthPoint<'_>) -> Result<()> {
    let at = point.name();
    hook(point).map_err(|e| anyhow::Error::from(e).context(format!("the hook at {at}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryo_v2_core::app::ports::Verifier;
    use ryo_v2_core::domain::{Place, StateFp};
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    /// The log the fake and its Layers share: the port calls, the drops of Layers, and whatever
    /// a test's hook adds.
    type Log = Arc<Mutex<Vec<String>>>;

    /// A job of the fake not yet delivered: its id and outcome.
    type Queued = (JobId, Result<Checked<FakeLayer>, CheckError>);

    /// A Layer of the fake: its depth over the root (0); it logs `drop(<depth>)` when dropped.
    #[derive(Debug)]
    struct FakeLayer {
        depth: usize,
        log: Log,
    }
    impl Clone for FakeLayer {
        fn clone(&self) -> FakeLayer {
            panic!("the series never clones a Layer")
        }
    }
    impl Drop for FakeLayer {
        fn drop(&mut self) {
            self.log
                .lock()
                .unwrap()
                .push(format!("drop({})", self.depth));
        }
    }

    /// Forks whose job at depth 1 diagnoses as `parent` and deeper as `top`, whose `layer_files`
    /// is `files`, with one job in the pool at a time, and which logs every `open`, `submit`,
    /// `next` and `close`.
    #[derive(Default)]
    struct FakeForks {
        parent: ScopedFindings,
        top: ScopedFindings,
        files: Vec<RelPath>,
        log: Log,
        queue: RefCell<VecDeque<Queued>>,
        next_id: Cell<u64>,
        /// Whether `close` fails as on a base a job changed (I2).
        poison_close: bool,
        /// Whether `close` fails as a session error.
        fail_close: bool,
        /// Whether a job above depth 1 is cut off.
        fail_top_diagnose: bool,
    }
    impl FakeForks {
        fn note(&self, entry: impl Into<String>) {
            self.log.lock().unwrap().push(entry.into());
        }
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut *self.log.lock().unwrap())
        }
        fn layer(&self, depth: usize) -> FakeLayer {
            FakeLayer {
                depth,
                log: self.log.clone(),
            }
        }
    }
    impl Verifier for FakeForks {
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
    impl Forks for FakeForks {
        type Layer = FakeLayer;
        fn open(&self, _: &mut ()) -> Result<FakeLayer, CheckError> {
            self.note("open");
            Ok(self.layer(0))
        }
        fn width(&self, _: &()) -> usize {
            1
        }
        fn submit(
            &self,
            _: &mut (),
            parent: &FakeLayer,
            edits: &[Edit],
            _: Scope,
            limit: Limit,
        ) -> Result<JobId, CheckError> {
            assert_eq!(limit, Limit::Check, "a level is cut off as a check is");
            assert!(self.queue.borrow().is_empty(), "one job in flight (W = 1)");
            let paths: Vec<String> = edits.iter().map(|e| e.path.to_string()).collect();
            self.note(format!("submit({})", paths.join(",")));
            let depth = parent.depth + 1;
            let found = if depth == 1 {
                Ok(self.parent.clone())
            } else if self.fail_top_diagnose {
                Err(CheckError::TimedOut { limit_ms: 7 })
            } else {
                Ok(self.top.clone())
            };
            let id = JobId::new(self.next_id.get() + 1);
            self.next_id.set(self.next_id.get() + 1);
            let result = found.map(|findings| Checked {
                layer: self.layer(depth),
                findings,
            });
            self.queue.borrow_mut().push_back((id, result));
            Ok(id)
        }
        fn expand(
            &self,
            _: &mut (),
            _: &FakeLayer,
            _: StateFp,
            _: &[Place],
            _: usize,
        ) -> Result<JobId, CheckError> {
            Err(CheckError::ForkInUse)
        }
        fn next(&self, _: &mut (), _: Duration) -> Result<Next<FakeLayer>, CheckError> {
            self.note("next");
            Ok(match self.queue.borrow_mut().pop_front() {
                Some((id, result)) => Next::Done(id, result),
                None => Next::Idle,
            })
        }
        fn cancel(&self, _: &mut (), _: JobId) -> Result<bool, CheckError> {
            Ok(false)
        }
        fn layer_files(&self, _: &(), _: &FakeLayer, _: Scope) -> Result<Vec<RelPath>, CheckError> {
            Ok(self.files.clone())
        }
        fn layer_text(
            &self,
            _: &(),
            _: &FakeLayer,
            _: &RelPath,
        ) -> Result<Option<String>, CheckError> {
            Ok(None)
        }
        fn close(&self, _: &mut ()) -> Result<(), CheckError> {
            // A job still in the pool is withdrawn, its Layer with it.
            self.queue.borrow_mut().clear();
            self.note("close");
            if self.poison_close {
                return Err(CheckError::Poisoned(
                    "a job changed the base (I2)".to_owned(),
                ));
            }
            if self.fail_close {
                return Err(CheckError::ForkInUse);
            }
            Ok(())
        }
    }

    /// The point's name, as the reports and the logs of these tests print it.
    fn name(point: Point<'_>) -> &'static str {
        point.name()
    }

    fn rel(p: &str) -> RelPath {
        RelPath::parse(p).unwrap()
    }

    /// A failed read of `/proc`, as `proc.rs` reports one.
    fn failed_read() -> ProcError {
        ProcError::Read {
            file: PathBuf::from("/proc/self/status"),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        }
    }

    /// Whether every Layer the fake made was dropped before the close: every `drop` precedes the
    /// last `close`.
    fn dropped_before_close(log: &[String]) -> bool {
        let close = log.iter().rposition(|e| e == "close");
        let last_drop = log.iter().rposition(|e| e.starts_with("drop("));
        matches!((close, last_drop), (Some(c), Some(d)) if d < c)
    }

    #[test]
    fn a_stacked_verdict_is_formed_as_the_search_forms_it() {
        use ryo_v2_core::domain::UnseenClass;
        use std::collections::{BTreeMap, BTreeSet};
        // The parent's level observed a class on b; the variant's level diagnoses a only.
        let below = ScopedFindings {
            files: vec![rel("a.rs"), rel("b.rs")],
            unseen_at: BTreeMap::from([(
                UnseenClass::UnresolvedPath,
                BTreeSet::from([rel("b.rs")]),
            )]),
            ..ScopedFindings::default()
        };
        let a = FakeForks {
            parent: below.clone(),
            top: ScopedFindings {
                files: vec![rel("a.rs")],
                ..ScopedFindings::default()
            },
            files: vec![rel("a.rs"), rel("b.rs")],
            ..FakeForks::default()
        };
        let parent = [Edit::write(rel("b.rs"), "fn b() {}")];
        let variant = [Edit::write(rel("a.rs"), "fn a() {}")];
        let mut seen: Vec<Verdict> = Vec::new();
        let isolated = stacked_series(
            &a,
            &mut (),
            "fake",
            &parent,
            &variant,
            Scope::Affected,
            1,
            &mut |point| {
                if let Point::AfterPop(v) = point {
                    seen.push(v.clone());
                }
                Ok(())
            },
        )
        .unwrap();
        assert!(isolated);
        let verdict = seen.pop();
        let label = crate::bench::label_of(verdict.as_ref());
        let want = Verdict::of_level(&[], &below, &a.top, &[], &a.files).0;
        assert_eq!(verdict, Some(want));
        // The class kept from below is named: the top level's diagnosis alone would not name it.
        let named = verdict.map(|v| v.unseen).unwrap_or_default();
        assert_eq!(named, BTreeSet::from([UnseenClass::UnresolvedPath]));
        assert!(Verdict::from_baseline(&[], &a.top).unseen.is_empty());
        assert_eq!(label, "PASS");
        assert!(dropped_before_close(&a.take()), "every level is dropped");
    }

    /// The log of one series run with the bench's hook or the probe's: the fake's port calls with
    /// the hook's points between them.
    fn run_logged(observe: &mut dyn FnMut(Point<'_>) -> Result<(), ProcError>) -> Vec<String> {
        let a = FakeForks {
            files: vec![rel("a.rs"), rel("p.rs")],
            ..FakeForks::default()
        };
        let parent = [Edit::write(rel("p.rs"), "fn p() {}")];
        let variant = [Edit::write(rel("a.rs"), "fn a() {}")];
        let isolated = stacked_series(
            &a,
            &mut (),
            "fake",
            &parent,
            &variant,
            Scope::Affected,
            2,
            &mut |point| {
                a.note(name(point));
                observe(point)
            },
        )
        .unwrap();
        assert!(isolated);
        a.take()
    }

    #[test]
    fn probe_and_bench_run_the_same_order() {
        // The bench's hook: the strict VmRSS read at ParentDiagnosed and AfterPop.
        let mut rss = Vec::new();
        let bench = run_logged(&mut |point| {
            if matches!(point, Point::ParentDiagnosed | Point::AfterPop(_)) {
                rss.push(crate::proc::vm_rss_kb()?);
            }
            Ok(())
        });
        // The probe's hook: a reading at every point.
        let mut readings = Vec::new();
        let probe = run_logged(&mut |point| {
            readings.push((name(point), crate::proc::vm_rss_kb()?));
            Ok(())
        });
        let mut want: Vec<String> = ["open", "submit(p.rs)", "next", "ParentDiagnosed"]
            .map(String::from)
            .to_vec();
        for _ in 0..2 {
            want.extend(
                [
                    "BeforeWrite",
                    "submit(a.rs)",
                    "AfterWrite",
                    "next",
                    "AfterDiagnose",
                    "BeforePop",
                    "drop(2)",
                    "AfterPop",
                ]
                .map(String::from),
            );
        }
        want.extend(["drop(1)", "drop(0)", "close"].map(String::from));
        assert_eq!(bench, want);
        assert_eq!(probe, bench);
        assert_eq!(rss.len(), 3);
        assert_eq!(readings.len(), 1 + 5 * 2);
        // The bench's own hook, the one `bench::run` passes, through the same series: the same
        // order, one recorded run per repeat, each with its VmRSS, and the parent's VmRSS.
        let (real, runs, rss_runs, rss_parent) = crate::bench::with_bench_hook(2, run_logged);
        assert_eq!(real, want);
        assert_eq!((runs, rss_runs), (2, 2));
        assert!(rss_parent.is_some());
    }

    #[test]
    fn a_failed_hook_read_ends_the_series() {
        let a = FakeForks::default();
        let parent = [Edit::write(rel("p.rs"), "fn p() {}")];
        let variant = [Edit::write(rel("a.rs"), "fn a() {}")];
        let result = stacked_series(
            &a,
            &mut (),
            "fake",
            &parent,
            &variant,
            Scope::Affected,
            3,
            &mut |point| match point {
                Point::AfterPop(_) => Err(failed_read()),
                _ => Ok(()),
            },
        );
        let e = result.unwrap_err();
        assert!(
            matches!(e.downcast_ref(), Some(ProcError::Read { .. })),
            "{e:#}"
        );
        let log = a.take();
        assert_eq!(log.last().map(String::as_str), Some("close"));
        assert!(dropped_before_close(&log), "{log:?}");
        // The first repeat's drop was the last level written: no second repeat ran.
        assert_eq!(log.iter().filter(|e| *e == "submit(a.rs)").count(), 1);
    }

    #[test]
    fn a_hook_error_names_the_failure_it_cuts_short() {
        let parent = [Edit::write(rel("p.rs"), "fn p() {}")];
        let variant = [Edit::write(rel("a.rs"), "fn a() {}")];
        // A cut-off diagnosis met by a hook failing at AfterDiagnose, and at BeforePop.
        for at in ["AfterDiagnose", "BeforePop"] {
            let a = FakeForks {
                fail_top_diagnose: true,
                ..FakeForks::default()
            };
            let result = stacked_series(
                &a,
                &mut (),
                "fake",
                &parent,
                &variant,
                Scope::Affected,
                2,
                &mut |point| {
                    if point.name() == at {
                        Err(failed_read())
                    } else {
                        Ok(())
                    }
                },
            );
            let e = result.unwrap_err();
            assert!(
                matches!(e.downcast_ref(), Some(ProcError::Read { .. })),
                "{e:#}"
            );
            let shown = format!("{e:#}");
            assert!(shown.contains(at), "{shown}");
            assert!(shown.contains("cut off after 7 ms"), "{shown}");
            assert_eq!(a.take().last().map(String::as_str), Some("close"));
        }
        // Without a hook error the failed diagnosis is the series' error.
        let a = FakeForks {
            fail_top_diagnose: true,
            ..FakeForks::default()
        };
        let e = stacked_series(
            &a,
            &mut (),
            "fake",
            &parent,
            &variant,
            Scope::Affected,
            2,
            &mut |_| Ok(()),
        )
        .unwrap_err();
        assert!(
            format!("{e:#}").contains("diagnose the variant's level"),
            "{e:#}"
        );
    }

    #[test]
    fn a_failed_close_keeps_the_series_error() {
        let a = FakeForks {
            fail_close: true,
            ..FakeForks::default()
        };
        let parent = [Edit::write(rel("p.rs"), "fn p() {}")];
        let variant = [Edit::write(rel("a.rs"), "fn a() {}")];
        let result = stacked_series(
            &a,
            &mut (),
            "fake",
            &parent,
            &variant,
            Scope::Affected,
            1,
            &mut |point| match point {
                Point::ParentDiagnosed => Err(failed_read()),
                _ => Ok(()),
            },
        );
        let e = result.unwrap_err();
        assert!(
            matches!(e.downcast_ref(), Some(ProcError::Read { .. })),
            "{e:#}"
        );
        assert_eq!(
            a.take(),
            [
                "open",
                "submit(p.rs)",
                "next",
                "drop(1)",
                "drop(0)",
                "close"
            ]
            .map(String::from)
        );
        // With no series error the failed close is the error.
        let a = FakeForks {
            fail_close: true,
            ..FakeForks::default()
        };
        let e = stacked_series(
            &a,
            &mut (),
            "fake",
            &parent,
            &variant,
            Scope::Affected,
            1,
            &mut |_| Ok(()),
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("close the forks"), "{e:#}");
    }

    #[test]
    fn a_poisoned_close_is_an_isolation_failure() {
        let a = FakeForks {
            poison_close: true,
            ..FakeForks::default()
        };
        let parent = [Edit::write(rel("p.rs"), "fn p() {}")];
        let variant = [Edit::write(rel("a.rs"), "fn a() {}")];
        let isolated = stacked_series(
            &a,
            &mut (),
            "fake",
            &parent,
            &variant,
            Scope::Affected,
            1,
            &mut |_| Ok(()),
        )
        .unwrap();
        assert!(!isolated);
        let files = [(rel("a.rs"), String::new())];
        let (result, _) = depth_logged(&a, &depth_levels(&files, 2), 1);
        assert!(!result.unwrap());
    }

    #[test]
    fn depth_levels_compose_one_edit_per_level() {
        let files = [
            (rel("a.rs"), "fn a() {}".to_owned()),
            (rel("b.rs"), String::new()),
        ];
        let levels = depth_levels(&files, 3);
        assert_eq!(levels.len(), 3);
        assert!(levels.iter().all(|l| l.len() == 1), "one edit per level");
        let paths: Vec<String> = levels.iter().map(|l| l[0].path.to_string()).collect();
        assert_eq!(paths, ["a.rs", "b.rs", "a.rs"]);
        let text = |k: usize| levels[k - 1][0].new_text.clone().unwrap();
        let f = |k: usize| format!("\nfn __ryo_depth_{k}() -> u32 {{\n    {k}\n}}\n");
        assert_eq!(text(1), format!("fn a() {{}}\n{}", f(1)));
        assert_eq!(text(2), f(2));
        // Level 3 is the composed text of a.rs: level 1's function, then its own.
        assert_eq!(text(3), format!("{}{}", text(1), f(3)));
        // On one file every level changes it from the level below.
        let one = depth_levels(&files[..1], 4);
        for w in one.windows(2) {
            assert_ne!(w[0][0].new_text, w[1][0].new_text);
            assert!(
                w[1][0]
                    .new_text
                    .as_deref()
                    .unwrap()
                    .starts_with(w[0][0].new_text.as_deref().unwrap())
            );
        }
        assert!(depth_levels(&[], 3).is_empty());
    }

    /// The log of one depth series on `a`, with every point the hook sees between the port calls.
    fn depth_logged(
        a: &FakeForks,
        levels: &[Vec<Edit>],
        repeat: u32,
    ) -> (Result<bool>, Vec<String>) {
        depth_logged_as(a, levels, DepthShape::Top(repeat))
    }

    /// As `depth_logged`, with the shape given; `on_stacked` logs `stacked(<Layers>)`.
    fn depth_logged_as(
        a: &FakeForks,
        levels: &[Vec<Edit>],
        shape: DepthShape,
    ) -> (Result<bool>, Vec<String>) {
        let result = depth_series(
            a,
            &mut (),
            "fake",
            levels,
            Scope::Affected,
            shape,
            &mut |point| {
                let k = match point {
                    DepthPoint::BeforeWrite(k)
                    | DepthPoint::AfterWrite(k)
                    | DepthPoint::BeforePop(k)
                    | DepthPoint::AfterPop(k) => k.to_string(),
                    DepthPoint::BeforeDiagnose(r) | DepthPoint::AfterDiagnose(r, _) => {
                        r.to_string()
                    }
                };
                a.note(format!("{}({k})", point.name()));
                Ok(())
            },
            &mut |_, layers| {
                let depths: Vec<String> = layers.iter().map(|l| l.depth.to_string()).collect();
                a.note(format!("stacked({})", depths.join(",")));
            },
        );
        (result, a.take())
    }

    #[test]
    fn an_every_level_series_diagnoses_each_level_before_the_next() {
        let a = FakeForks::default();
        let files = [(rel("a.rs"), String::new()), (rel("b.rs"), String::new())];
        let (result, log) = depth_logged_as(&a, &depth_levels(&files, 2), DepthShape::EveryLevel);
        assert!(result.unwrap());
        let want = [
            "open",
            "BeforeWrite(1)",
            "submit(a.rs)",
            "AfterWrite(1)",
            "BeforeDiagnose(1)",
            "next",
            "AfterDiagnose(1)",
            "BeforeWrite(2)",
            "submit(b.rs)",
            "AfterWrite(2)",
            "BeforeDiagnose(2)",
            "next",
            "AfterDiagnose(2)",
            "stacked(1,2)",
            "BeforePop(2)",
            "drop(2)",
            "AfterPop(2)",
            "BeforePop(1)",
            "drop(1)",
            "AfterPop(1)",
            "drop(0)",
            "close",
        ]
        .map(String::from);
        assert_eq!(log, want);
    }

    #[test]
    fn a_depth_series_writes_k_levels_and_pops_k_in_order() {
        let a = FakeForks::default();
        let files = [
            (rel("a.rs"), String::new()),
            (rel("b.rs"), String::new()),
            (rel("c.rs"), String::new()),
        ];
        let levels = depth_levels(&files, 3);
        let (result, log) = depth_logged(&a, &levels, 2);
        assert!(result.unwrap());
        let want = [
            "open",
            "BeforeWrite(1)",
            "submit(a.rs)",
            "next",
            "AfterWrite(1)",
            "BeforeWrite(2)",
            "submit(b.rs)",
            "next",
            "AfterWrite(2)",
            "BeforeWrite(3)",
            "submit(c.rs)",
            "AfterWrite(3)",
            "BeforeDiagnose(1)",
            "next",
            "AfterDiagnose(1)",
            // Diagnosis 2: the top level again, on level 2's Layer, then dropped.
            "BeforeDiagnose(2)",
            "submit(c.rs)",
            "next",
            "AfterDiagnose(2)",
            "drop(3)",
            "stacked(1,2,3)",
            "BeforePop(3)",
            "drop(3)",
            "AfterPop(3)",
            "BeforePop(2)",
            "drop(2)",
            "AfterPop(2)",
            "BeforePop(1)",
            "drop(1)",
            "AfterPop(1)",
            "drop(0)",
            "close",
        ]
        .map(String::from);
        assert_eq!(log, want);
        // The levels written are the levels given, in order.
        let (_, log) = depth_logged(&a, &depth_levels(&files[..1], 2), 1);
        let submits = log.iter().filter(|e| e.starts_with("submit(")).count();
        let pops = log.iter().filter(|e| e.starts_with("AfterPop(")).count();
        assert_eq!((submits, pops), (2, 2));
    }

    #[test]
    fn a_depth_series_stops_at_a_failure_and_closes() {
        let files = [(rel("a.rs"), String::new())];
        let levels = depth_levels(&files, 2);
        // A failed diagnosis of the top level: no pop, the close follows every drop.
        let a = FakeForks {
            fail_top_diagnose: true,
            ..FakeForks::default()
        };
        let (result, log) = depth_logged(&a, &levels, 3);
        let e = result.unwrap_err();
        assert!(
            format!("{e:#}").contains("diagnose the top level (1/3)"),
            "{e:#}"
        );
        assert!(
            !log.iter()
                .any(|e| e.starts_with("BeforePop") || e.starts_with("stacked")),
            "{log:?}"
        );
        assert_eq!(log.last().map(String::as_str), Some("close"));
        assert!(dropped_before_close(&log), "{log:?}");
        // A hook error ends the series with it; the forks are still closed.
        let a = FakeForks::default();
        let result = depth_series(
            &a,
            &mut (),
            "fake",
            &levels,
            Scope::Affected,
            DepthShape::Top(1),
            &mut |point| match point {
                DepthPoint::AfterWrite(2) => Err(failed_read()),
                _ => Ok(()),
            },
            &mut |_, _| panic!("a series cut short calls no on_stacked"),
        );
        let e = result.unwrap_err();
        assert!(
            matches!(e.downcast_ref(), Some(ProcError::Read { .. })),
            "{e:#}"
        );
        assert!(format!("{e:#}").contains("AfterWrite"));
        let log = a.take();
        assert_eq!(log.last().map(String::as_str), Some("close"));
        assert!(dropped_before_close(&log), "{log:?}");
    }
}
