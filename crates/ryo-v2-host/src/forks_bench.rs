//! `bench --forks`: N live forks of the fork Engine side by side through the `Forks` port (D11,
//! stage 3b), measured against one fork at a time.
//!
//! One job tree ([`job_tree`]) is run at every width asked for: a seed job on the base's root,
//! then `depth` levels of `jobs` sibling jobs each, every sibling of level k on the sealed layer
//! of job 0 of level k − 1 (the seed's for level 1). A level is submitted at once as soon as its
//! parent is sealed, so the pool is kept full when a level has at least `width` jobs. Every job is
//! one write of one workspace file, an item edit (a function appended) or a body edit (the
//! literal of the function the seed appended changed), rotating over the files as
//! `stacked::depth_levels` does. Only the chain of parents is kept; every other job's layer is
//! dropped when its outcome is read, so the live layers are at most `depth + 1` sealed ones and
//! the `width` in flight ([`tree_bound_bytes`], from Table M4 of the ledger).
//!
//! Per width (`ForksConfig.width`, and `caller_works` for a width written `<n>c`) it reports the
//! jobs per second from the first submit of level 1 to the last outcome, the speedup against
//! W = 1, the check time of each job (its `Event::Checked`: its writes and its diagnosis, from its
//! start on a worker; a cut-off job, which records no event, counted at its cut-off, a lower
//! bound), the busy workers over that time, the CPU occupancy, the peak RSS and threads of the
//! process, the check times split by Affected scope size (< 8 and ≥ 8 files; cut-off jobs left
//! out, their scope not being known), and whether every job's outcome is that of W = 1 (W4: by
//! node, the findings of a `Done`, or the kind and file set of a failure; [`w4_diff`],
//! [`w4_sum`]). With the cut-off off, as W4 runs, an incomplete comparison fails W4 too: no row
//! compared, a row left out, a run that met the RSS stop or submitted fewer jobs than the tree
//! has ([`W4::fails`]). After every outcome the RSS is read; above the machine's RAM less the
//! reserve (`--rss-stop-gib`) the run withdraws every job in flight, closes the forks and
//! reports the row as stopped. Repeat r runs the widths from width (r − 1) mod n round
//! ([`width_order`]), after one untimed warm-up at the first width and, with `--fork-serial`,
//! one of the serial run.

use crate::bench::scope_name;
use crate::layer_cost::{EditClass, Quantiles};
use crate::proc;
use anyhow::{Context, Result};
use ryo_v2_core::app::ports::{CheckError, Event, Forks, JobId, Limit, Next, Verifier};
use ryo_v2_core::domain::{Edit, RelPath, Scope, ScopedFindings};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_ra::{ForksConfig, RaAnalyzer, RaConfig};
use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The function the seed job appends to every file of the tree; a body job changes its literal.
pub const FORK_BODY_FN: &str = "__ryo_fork_body";

/// The scope size, in files, at which the report splits the jobs (the gate X3b's split).
pub const SCOPE_SPLIT: usize = 8;

/// CONCEPT.md Core 4: the check time a job is counted against, in ms.
pub const CORE4_MS: u64 = 5_000;

/// The most one live layer of a tree keeps, in bytes, from Table M4 of the ledger: the worst
/// level measured (a wide4 item level at 1 thread, 52.175 MB kept) plus its chain view at depth
/// 20 (1.516 MB).
pub const LAYER_BYTES_WORST: u64 = 53_691_000;

/// The most one running job holds above what it keeps, in bytes, from Table M4: the worst
/// transient measured (a wide4 item level at 12 threads, 9.867 MB).
pub const JOB_TRANSIENT_BYTES_WORST: u64 = 9_867_000;

/// A width of `--forks`: N, the jobs asked to run at once (`ForksConfig.width`), and whether the
/// caller's thread is counted out of the workers' share (`ForksConfig.caller_works`), written
/// `<n>c`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WidthSpec {
    /// N.
    pub n: NonZeroUsize,
    /// `ForksConfig.caller_works`.
    pub caller_works: bool,
}

impl WidthSpec {
    /// Parses `<n>` (the bench's caller only waits: `caller_works` false) or `<n>c` (stage 4's
    /// shape: `caller_works` true).
    ///
    /// # Contract
    /// - C1: `<n>` with n ≥ 1 is N = n with `caller_works` false, `<n>c` the same with it true;
    ///   0, a sign, a space, another suffix or no number is an error naming the text.
    ///   tested: forks_bench::tests::widths_parse_with_the_caller_suffix
    pub fn parse(text: &str) -> Result<WidthSpec, String> {
        let (num, caller_works) = match text.strip_suffix('c') {
            Some(num) => (num, true),
            None => (text, false),
        };
        if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!(
                "{text:?}: a width is a number, optionally followed by `c`"
            ));
        }
        let n: usize = num.parse().map_err(|e| format!("{text:?}: {e}"))?;
        let n = NonZeroUsize::new(n).ok_or_else(|| format!("{text:?}: a width is at least 1"))?;
        Ok(WidthSpec { n, caller_works })
    }

    /// The width as `--forks` writes it.
    pub fn label(self) -> String {
        format!("{}{}", self.n, if self.caller_works { "c" } else { "" })
    }
}

/// The width W and the threads t of one job's diagnosis that the adapter's `Forks` gives a width
/// of `spec` on a session of `threads` threads (`ForksConfig`): `T_w = max(1, T − c)`,
/// `W = min(N, T_w)`, `t = max(1, T_w / W)`. The bench checks W against `Forks::width`.
///
/// # Contract
/// - C1: the formula above, with c = 1 for `caller_works`; on 12 threads 1 gives (1, 12), 4 gives
///   (4, 3), 8 gives (8, 1), 12 gives (12, 1), 16 gives (12, 1), and `12c` and `11c` give (11, 1).
///   tested: forks_bench::tests::the_shape_follows_the_budget
pub fn shape(spec: WidthSpec, threads: usize) -> (usize, usize) {
    let workers = threads
        .max(1)
        .saturating_sub(usize::from(spec.caller_works))
        .max(1);
    let width = spec.n.get().min(workers);
    (width, (workers / width).max(1))
}

/// One job of a [`JobTree`]: its level (from 1), its index among its siblings (job 0 is the
/// parent of the next level), its class and its edits (one write of one file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeJob {
    /// The level, from 1.
    pub level: usize,
    /// The index among the siblings of the level, from 0.
    pub index: usize,
    /// Body or item.
    pub class: EditClass,
    /// One write of one file, its whole text at the parent with the job's change.
    pub edits: Vec<Edit>,
}

/// The jobs of one tree: the seed, written on the base's root, and the levels; every job of
/// level k is written on the layer of job 0 of level k − 1 (the seed's for level 1).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JobTree {
    /// The seed: [`FORK_BODY_FN`] returning 0 appended to every file.
    pub seed: Vec<Edit>,
    /// The levels, from level 1.
    pub levels: Vec<Vec<TreeJob>>,
}

impl JobTree {
    /// The number of jobs of the levels (the seed not counted).
    pub fn jobs(&self) -> usize {
        self.levels.iter().map(Vec::len).sum()
    }
}

/// The text of [`FORK_BODY_FN`] returning `v`, as the tree appends it.
fn body_fn(v: usize) -> String {
    format!("\nfn {FORK_BODY_FN}() -> u32 {{\n    {v}\n}}\n")
}

/// The job tree of `depth` levels of `jobs` siblings over `files` (each a path and its base
/// text). The seed appends [`FORK_BODY_FN`] returning 0 to every file. Job j of level k writes
/// file `((k − 1) · jobs + j) % files.len()`; it is an item job (it appends
/// `fn __ryo_fork_<k>_<j>() -> u32`) when `(j / files.len())` is even and a body job (it changes
/// the literal [`FORK_BODY_FN`] returns) when it is odd, so both classes meet every file. Each
/// job's text is its file's text at its parent (the seed and the chain of jobs 0 below it) with
/// that one change, the value written being unique in the tree. No files, depth or jobs: no tree.
///
/// # Contract
/// - C1: the seed is one write per file, its base text with the body function appended; level k
///   has `jobs` jobs, each one write of the file above, whose text is that file's text at the
///   parent (the seed's writes, then the writes of job 0 of every level below k) with one item
///   appended or the body function's literal changed, as the class says; no job repeats its
///   parent's text, and no two jobs write the same text.
///   tested: forks_bench::tests::the_tree_writes_one_change_on_its_parent
pub fn job_tree(files: &[(RelPath, String)], depth: usize, jobs: usize) -> JobTree {
    if files.is_empty() || depth == 0 || jobs == 0 {
        return JobTree::default();
    }
    let mut texts: Vec<String> = files
        .iter()
        .map(|(_, t)| {
            let mut text = t.clone();
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&body_fn(0));
            text
        })
        .collect();
    let seed = files
        .iter()
        .zip(&texts)
        .map(|((p, _), t)| Edit::write(p.clone(), t.clone()))
        .collect();
    let mut levels = Vec::with_capacity(depth);
    for k in 1..=depth {
        let level: Vec<TreeJob> = (0..jobs)
            .map(|j| {
                let i = ((k - 1) * jobs + j) % files.len();
                let v = (k - 1) * jobs + j + 1;
                let text = &texts[i];
                // Job 0 is an item job, so the chain below never changes the body function:
                // it returns the seed's 0 at every parent.
                let (class, new) = if (j / files.len()) % 2 == 1 {
                    (EditClass::Body, text.replacen(&body_fn(0), &body_fn(v), 1))
                } else {
                    let item = format!("{text}\nfn __ryo_fork_{k}_{j}() -> u32 {{\n    {v}\n}}\n");
                    (EditClass::Item, item)
                };
                TreeJob {
                    level: k,
                    index: j,
                    class,
                    edits: vec![Edit::write(files[i].0.clone(), new)],
                }
            })
            .collect();
        // Job 0 is the parent of the next level: its file's text there.
        let i0 = ((k - 1) * jobs) % files.len();
        if let Some(text) = level
            .first()
            .and_then(|job| job.edits.first())
            .and_then(|e| e.new_text.clone())
        {
            texts[i0] = text;
        }
        levels.push(level);
    }
    JobTree { seed, levels }
}

/// The most heap a tree of `depth` levels at width `width` holds at once by Table M4's worst
/// figures: `depth + 2` sealed layers (the seed, the chain of parents, the one outcome being
/// read) plus the `width` jobs in flight, each [`LAYER_BYTES_WORST`], and the transient of every
/// running job.
///
/// # Contract
/// - C1: `(depth + 2 + width) · LAYER_BYTES_WORST + width · JOB_TRANSIENT_BYTES_WORST`.
///   tested: forks_bench::tests::the_bound_counts_the_chain_and_the_jobs_in_flight
pub fn tree_bound_bytes(depth: usize, width: usize) -> u64 {
    (depth as u64 + 2 + width as u64) * LAYER_BYTES_WORST + width as u64 * JOB_TRANSIENT_BYTES_WORST
}

/// The RSS, in kB, above which a run stops: the machine's RAM less `reserve_gib` GiB.
///
/// # Contract
/// - C1: `mem_total_kb − reserve_gib · 2^20`, saturating at 0; a run stops at an RSS above it.
///   tested: forks_bench::tests::the_rss_stop_is_ram_less_the_reserve
pub fn rss_stop_kb(mem_total_kb: u64, reserve_gib: f64) -> u64 {
    let reserve_kb = (reserve_gib.max(0.0) * (1u64 << 20) as f64) as u64;
    mem_total_kb.saturating_sub(reserve_kb)
}

/// The outcome of one job as W4 compares it: the findings of a `Done` (files, findings and
/// unseen classes; no id), or the kind and file set of a failure; a withdrawal is not compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    /// The job ran to its end.
    Done(ScopedFindings),
    /// Its check could not finish: the error's kind and the files it names.
    Failed {
        /// The variant of the `CheckError`.
        kind: &'static str,
        /// The files the error names, sorted.
        paths: Vec<RelPath>,
    },
    /// The job was withdrawn (the RSS stop).
    Withdrawn,
}

impl JobOutcome {
    /// The outcome of a `Done`.
    pub fn of(result: Result<&ScopedFindings, &CheckError>) -> JobOutcome {
        match result {
            Ok(found) => JobOutcome::Done(found.clone()),
            Err(e) => {
                let (kind, mut paths) = failure(e);
                paths.sort();
                JobOutcome::Failed { kind, paths }
            }
        }
    }

    /// Whether the outcome is a cut-off check (a wall-clock effect).
    pub fn timed_out(&self) -> bool {
        matches!(
            self,
            JobOutcome::Failed {
                kind: "TimedOut",
                ..
            }
        )
    }
}

/// The kind of `e` and the files it names: what W4 compares of a failure (its message and order
/// depend on how the files were split among threads, which differs from run to run).
fn failure(e: &CheckError) -> (&'static str, Vec<RelPath>) {
    match e {
        CheckError::Edit { path, .. } => ("Edit", vec![path.clone()]),
        CheckError::Cancelled => ("Cancelled", Vec::new()),
        CheckError::Panicked(_) => ("Panicked", Vec::new()),
        CheckError::RevertMismatch { paths } => ("RevertMismatch", paths.clone()),
        CheckError::Poisoned(_) => ("Poisoned", Vec::new()),
        CheckError::TimedOut { .. } => ("TimedOut", Vec::new()),
        CheckError::ForkInUse => ("ForkInUse", Vec::new()),
        CheckError::AlreadyApplied => ("AlreadyApplied", Vec::new()),
        CheckError::Load(_) => ("Load", Vec::new()),
        CheckError::DetectorPanicked { file, .. } => ("DetectorPanicked", vec![file.clone()]),
    }
}

/// A job of the tree: (level, index).
pub type Node = (usize, usize);

/// What W4 found between a run and the reference.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct W4 {
    /// The nodes compared.
    pub compared: usize,
    /// The nodes whose outcomes differ.
    pub differ: Vec<Node>,
    /// The nodes of either side that were not compared: withdrawn, run on one side only, or
    /// (with `skip_timed_out`) cut off on either side.
    pub left_out: usize,
    /// The runs, the reference among them, that met the RSS stop.
    pub stopped: usize,
    /// The runs, the reference among them, that submitted fewer jobs than the tree has (a job
    /// 0 that failed or was withdrawn ends its subtree, on both sides alike).
    pub short: usize,
}

impl W4 {
    /// Whether every node of the tree was compared on every run: rows compared, none left out,
    /// no run stopped or short.
    pub fn complete(&self) -> bool {
        self.compared > 0 && self.left_out == 0 && self.stopped == 0 && self.short == 0
    }

    /// Whether W4 fails: a difference, or, when `strict` (the cut-off off, as W4 runs), a
    /// comparison that is not [`complete`](W4::complete).
    ///
    /// # Contract
    /// - C1: a difference always fails; with `strict`, also no row compared, a row left out, a
    ///   run stopped or a run short; without it those are only printed.
    ///   tested: forks_bench::tests::w4_fails_when_incomplete_with_the_cut_off_off
    pub fn fails(&self, strict: bool) -> bool {
        !self.differ.is_empty() || (strict && !self.complete())
    }

    /// `DIFF`, `INCOMPLETE` (failing only when `strict`) or `equal`.
    pub fn verdict(&self, strict: bool) -> &'static str {
        if !self.differ.is_empty() {
            "DIFF"
        } else if strict && !self.complete() {
            "INCOMPLETE"
        } else {
            "equal"
        }
    }
}

/// One run as W4 sums it: its outcomes, whether it met the RSS stop, and the jobs it submitted.
#[derive(Debug, Clone, Copy)]
pub struct Side<'a> {
    /// Each job's outcome.
    pub outcomes: &'a BTreeMap<Node, JobOutcome>,
    /// Whether the RSS stop withdrew its jobs.
    pub rss_stopped: bool,
    /// The jobs it submitted.
    pub submitted: usize,
}

/// W4 of every run of `runs` against `reference`, summed ([`w4_diff`] node by node), with the
/// runs that stopped or are short of the tree's `jobs` counted, the reference's once when there
/// is a run to compare.
///
/// # Contract
/// - C1: `compared`, `differ` and `left_out` are the sums of `w4_diff` over `runs`; `stopped`
///   and `short` count the runs, and the reference once when `runs` is not empty, that met the
///   RSS stop or submitted fewer than `jobs` jobs.
///   tested: forks_bench::tests::w4_fails_when_incomplete_with_the_cut_off_off
pub fn w4_sum(reference: Side<'_>, runs: &[Side<'_>], jobs: usize, skip_timed_out: bool) -> W4 {
    let mut sum = W4::default();
    let count = |side: &Side<'_>, sum: &mut W4| {
        sum.stopped += usize::from(side.rss_stopped);
        sum.short += usize::from(side.submitted < jobs);
    };
    for run in runs {
        let one = w4_diff(reference.outcomes, run.outcomes, skip_timed_out);
        sum.compared += one.compared;
        sum.left_out += one.left_out;
        sum.differ.extend(one.differ);
        count(run, &mut sum);
    }
    if !runs.is_empty() {
        count(&reference, &mut sum);
    }
    sum
}

/// The order of the widths in repeat `r` (from 0) of `n` widths: from `r % n` round.
///
/// # Contract
/// - C1: every index of `0..n` once, starting at `r % n` and wrapping; none for `n` = 0.
///   tested: forks_bench::tests::the_width_order_rotates_per_repeat
pub fn width_order(n: usize, r: usize) -> Vec<usize> {
    (0..n).map(|k| (r + k) % n).collect()
}

/// W4 of `run` against `reference`, node by node: equal outcomes (a `Done`'s findings, a
/// failure's kind and files). A node withdrawn or run on one side only is left out, and with
/// `skip_timed_out` (a run with the cut-off on) one cut off on either side too.
///
/// # Contract
/// - C1: every node present on both sides and withdrawn on neither (nor cut off, with
///   `skip_timed_out`) is compared, and it differs when the outcomes are not equal; every other
///   node of either side is counted as left out.
///   tested: forks_bench::tests::w4_compares_by_node_and_leaves_out_withdrawals
pub fn w4_diff(
    reference: &BTreeMap<Node, JobOutcome>,
    run: &BTreeMap<Node, JobOutcome>,
    skip_timed_out: bool,
) -> W4 {
    let mut w4 = W4::default();
    let skip = |o: &JobOutcome| *o == JobOutcome::Withdrawn || (skip_timed_out && o.timed_out());
    for (node, want) in reference {
        match run.get(node) {
            Some(got) if !skip(want) && !skip(got) => {
                w4.compared += 1;
                if got != want {
                    w4.differ.push(*node);
                }
            }
            _ => w4.left_out += 1,
        }
    }
    w4.left_out += run.keys().filter(|n| !reference.contains_key(n)).count();
    w4
}

/// What one run of the tree at one width measured.
#[derive(Debug, Clone, Default)]
pub struct TreeRun {
    /// W, as `Forks::width` gives it.
    pub width: usize,
    /// The jobs submitted (the seed not counted).
    pub submitted: usize,
    /// The jobs that ran to a `Done` with a layer.
    pub done: usize,
    /// The jobs whose `Done` is an error, the cut-off ones included.
    pub failed: usize,
    /// The jobs cut off (`TimedOut`).
    pub timed_out: usize,
    /// The jobs withdrawn.
    pub withdrawn: usize,
    /// The seed job's time from its submit to its outcome.
    pub seed_ms: f64,
    /// From the first submit of level 1 to the last outcome.
    pub wall_ms: f64,
    /// The process CPU time over `wall_ms`.
    pub cpu_ms: u64,
    /// Each job's `Event::Checked`: (scope files, check ms).
    pub checks: Vec<(usize, u64)>,
    /// The cut-off of each job cut off (`TimedOut`), in ms: a lower bound of its check time.
    /// A cut-off job records no `Event::Checked`, so its scope size is not known.
    pub cut_ms: Vec<u64>,
    /// The largest VmRSS read after an outcome.
    pub peak_rss_kb: u64,
    /// The most threads of the process read after an outcome.
    pub peak_threads: u64,
    /// Whether the RSS stop withdrew the jobs in flight.
    pub rss_stopped: bool,
    /// Each job's outcome.
    pub outcomes: BTreeMap<Node, JobOutcome>,
}

impl TreeRun {
    /// The jobs that finished (`Done` with a layer or an error) per second of `wall_ms`.
    pub fn jobs_per_s(&self) -> f64 {
        if self.wall_ms <= 0.0 {
            return 0.0;
        }
        (self.done + self.failed) as f64 / (self.wall_ms / 1000.0)
    }

    /// The check time of every job that ended or was cut off, in ms: each `Event::Checked`, and
    /// each cut-off job counted at its cut-off (a lower bound).
    pub fn check_times(&self) -> impl Iterator<Item = u64> + '_ {
        self.checks
            .iter()
            .map(|&(_, ms)| ms)
            .chain(self.cut_ms.iter().copied())
    }

    /// The workers busy on a check, on average over `wall_ms`: the checks' time over it, a
    /// cut-off job counted at its cut-off.
    pub fn busy_workers(&self) -> f64 {
        if self.wall_ms <= 0.0 {
            return 0.0;
        }
        self.check_times().map(|ms| ms as f64).sum::<f64>() / self.wall_ms
    }

    /// The run as W4 sums it.
    pub fn side(&self) -> Side<'_> {
        Side {
            outcomes: &self.outcomes,
            rss_stopped: self.rss_stopped,
            submitted: self.submitted,
        }
    }
}

/// The figures of one width over its repeats, as its summary line prints them.
#[derive(Debug, Clone, PartialEq)]
pub struct WidthSummary {
    /// The scope's name.
    pub scope: &'static str,
    /// The width asked for.
    pub spec: WidthSpec,
    /// W and t.
    pub width: usize,
    /// The threads of one job's diagnosis.
    pub job_threads: usize,
    /// N above W: the width was capped at the workers' share.
    pub capped: bool,
    /// W · t + c above the cores: more threads busy than the machine has.
    pub oversubscribed: bool,
    /// The repeats.
    pub repeats: usize,
    /// The jobs of one tree.
    pub jobs: usize,
    /// The median over the repeats of their jobs per second.
    pub jobs_per_s: f64,
    /// `jobs_per_s` over W = 1's; `None` without a W = 1 width.
    pub speedup: Option<f64>,
    /// Every repeat's check ms, a cut-off job counted at its cut-off (a lower bound).
    pub check_ms: Quantiles,
    /// The cut-off jobs counted in `check_ms` at their cut-off.
    pub cut_counted: usize,
    /// The check ms of the jobs of scope below [`SCOPE_SPLIT`] files (cut-off jobs, whose scope
    /// is not known, not counted).
    pub small: Quantiles,
    /// The check ms of the jobs of scope at least [`SCOPE_SPLIT`] files (cut-off jobs not
    /// counted).
    pub large: Quantiles,
    /// The median over the repeats of the busy workers.
    pub busy_workers: f64,
    /// The median over the repeats of the CPU occupancy (CPU ms over wall ms times the cores).
    pub cpu_occupancy: f64,
    /// The largest RSS of the repeats.
    pub peak_rss_kb: u64,
    /// The most threads of the repeats.
    pub peak_threads: u64,
    /// The cut-off jobs of every repeat.
    pub timed_out: usize,
    /// The checks above [`CORE4_MS`] of every repeat, a cut-off job counted at its cut-off.
    pub over_core4: usize,
    /// The withdrawn jobs of every repeat.
    pub withdrawn: usize,
    /// Whether a repeat met the RSS stop.
    pub rss_stopped: bool,
    /// W4 of every repeat against the reference, summed; `None` for the reference itself.
    pub w4: Option<W4>,
    /// Whether W4 is strict (the cut-off off): an incomplete comparison fails ([`W4::fails`]).
    pub w4_strict: bool,
}

/// The median of `xs`, the lower one for an even count (as `bench::median`); 0 for none.
fn median_or_zero(xs: &[f64]) -> f64 {
    crate::bench::median(xs).unwrap_or(0.0)
}

impl WidthSummary {
    /// The summary of the repeats `runs` of `spec`, on `threads` threads of a machine of `cores`
    /// cores; `base_rate` is W = 1's jobs per second, `w4` the W4 of every repeat summed, strict
    /// when `w4_strict`.
    #[allow(clippy::too_many_arguments)]
    pub fn of(
        scope: Scope,
        spec: WidthSpec,
        threads: usize,
        cores: usize,
        jobs: usize,
        runs: &[TreeRun],
        base_rate: Option<f64>,
        w4: Option<W4>,
        w4_strict: bool,
    ) -> WidthSummary {
        let (width, job_threads) = shape(spec, threads);
        let rates: Vec<f64> = runs.iter().map(TreeRun::jobs_per_s).collect();
        let jobs_per_s = median_or_zero(&rates);
        let all = |keep: &dyn Fn(usize) -> bool| -> Quantiles {
            let xs: Vec<i64> = runs
                .iter()
                .flat_map(|r| r.checks.iter())
                .filter(|(files, _)| keep(*files))
                .map(|&(_, ms)| ms as i64)
                .collect();
            Quantiles::of(&xs)
        };
        let busy: Vec<f64> = runs.iter().map(TreeRun::busy_workers).collect();
        let cpu: Vec<f64> = runs
            .iter()
            .map(|r| crate::bench::occupancy(r.cpu_ms, r.wall_ms, cores.max(1)))
            .collect();
        WidthSummary {
            scope: scope_name(scope),
            spec,
            width,
            job_threads,
            capped: spec.n.get() > width,
            oversubscribed: width * job_threads + usize::from(spec.caller_works) > cores,
            repeats: runs.len(),
            jobs,
            jobs_per_s,
            speedup: base_rate.filter(|r| *r > 0.0).map(|r| jobs_per_s / r),
            check_ms: Quantiles::of(
                &runs
                    .iter()
                    .flat_map(TreeRun::check_times)
                    .map(|ms| ms as i64)
                    .collect::<Vec<_>>(),
            ),
            cut_counted: runs.iter().map(|r| r.cut_ms.len()).sum(),
            small: all(&|f| f < SCOPE_SPLIT),
            large: all(&|f| f >= SCOPE_SPLIT),
            busy_workers: median_or_zero(&busy),
            cpu_occupancy: median_or_zero(&cpu),
            peak_rss_kb: runs.iter().map(|r| r.peak_rss_kb).max().unwrap_or(0),
            peak_threads: runs.iter().map(|r| r.peak_threads).max().unwrap_or(0),
            timed_out: runs.iter().map(|r| r.timed_out).sum(),
            over_core4: runs
                .iter()
                .flat_map(TreeRun::check_times)
                .filter(|ms| *ms > CORE4_MS)
                .count(),
            withdrawn: runs.iter().map(|r| r.withdrawn).sum(),
            rss_stopped: runs.iter().any(|r| r.rss_stopped),
            w4,
            w4_strict,
        }
    }
}

/// `n min/p50/p90/max` of check ms, `0 -` for none.
fn quantiles_txt(q: &Quantiles) -> String {
    if q.n == 0 {
        return "0 -".to_owned();
    }
    format!("{} {}/{}/{}/{}", q.n, q.min, q.p50, q.p90, q.max)
}

/// The summary line of one width.
///
/// # Contract
/// - C1: `summary forks [<scope>] width <label> W <w> t <t> caller_works <b> capped <b>
///   oversubscribed <b> n=<repeats> jobs <J> jobs_per_s <x.xx> speedup <x.xx|-> check_ms <n>
///   <min/p50/p90/max> cut_at_limit <n> scope_lt8 <n> <q> scope_ge8 <n> <q> busy_workers <x.xx>
///   occupancy <busy/W, x.xx> cpu_occupancy <x.xx> peak_rss_kb <kb> peak_threads <n> timed_out
///   <n> over_5000ms <n> withdrawn <n> rss_stop <b> w4 <equal|DIFF|INCOMPLETE|ref> rows <n>
///   diffs <n> left_out <n> stopped <n> short <n>`; `check_ms` and `over_5000ms`
///   count each cut-off job at its cut-off (a lower bound; `cut_at_limit` of them), the scope
///   splits leave them out; a set of no checks prints `0 -`, the verdict is [`W4::verdict`], and
///   the reference prints `w4 ref rows - diffs - left_out - stopped - short -`.
///   tested: forks_bench::tests::the_summary_line_names_every_figure
pub fn summary_line(s: &WidthSummary) -> String {
    let speedup = s.speedup.map_or("-".to_owned(), |x| format!("{x:.2}"));
    let w4 = match &s.w4 {
        None => "ref rows - diffs - left_out - stopped - short -".to_owned(),
        Some(w) => format!(
            "{} rows {} diffs {} left_out {} stopped {} short {}",
            w.verdict(s.w4_strict),
            w.compared,
            w.differ.len(),
            w.left_out,
            w.stopped,
            w.short
        ),
    };
    let occupancy = if s.width == 0 {
        0.0
    } else {
        s.busy_workers / s.width as f64
    };
    format!(
        "summary forks [{}] width {} W {} t {} caller_works {} capped {} oversubscribed {} \
         n={} jobs {} jobs_per_s {:.2} speedup {speedup} check_ms {} cut_at_limit {} \
         scope_lt{SCOPE_SPLIT} {} \
         scope_ge{SCOPE_SPLIT} {} busy_workers {:.2} occupancy {occupancy:.2} cpu_occupancy {:.2} \
         peak_rss_kb {} peak_threads {} timed_out {} over_{CORE4_MS}ms {} withdrawn {} rss_stop \
         {} w4 {w4}",
        s.scope,
        s.spec.label(),
        s.width,
        s.job_threads,
        s.spec.caller_works,
        s.capped,
        s.oversubscribed,
        s.repeats,
        s.jobs,
        s.jobs_per_s,
        quantiles_txt(&s.check_ms),
        s.cut_counted,
        quantiles_txt(&s.small),
        quantiles_txt(&s.large),
        s.busy_workers,
        s.cpu_occupancy,
        s.peak_rss_kb,
        s.peak_threads,
        s.timed_out,
        s.over_core4,
        s.withdrawn,
        s.rss_stopped,
    )
}

/// The line of one run (one repeat of one width) of a tree of `jobs` jobs.
fn run_line(
    sname: &str,
    spec: WidthSpec,
    r: usize,
    of: usize,
    jobs: usize,
    run: &TreeRun,
) -> String {
    format!(
        "forks [{sname}] width {} {r}/{of}: W {} submitted {}/{jobs} done {} failed {} timed_out \
         {} withdrawn {} seed_ms {:.0} wall_ms {:.0} jobs_per_s {:.2} busy_workers {:.2} cpu_ms {} \
         peak_rss_kb {} peak_threads {} rss_stop {}",
        spec.label(),
        run.width,
        run.submitted,
        run.done,
        run.failed,
        run.timed_out,
        run.withdrawn,
        run.seed_ms,
        run.wall_ms,
        run.jobs_per_s(),
        run.busy_workers(),
        run.cpu_ms,
        run.peak_rss_kb,
        run.peak_threads,
        run.rss_stopped,
    )
}

/// How long one `Forks::next` waits before the bench reads the RSS again.
const NEXT_TICK: Duration = Duration::from_millis(250);

/// Reads the RSS and the threads into `run`; true when the RSS is above `stop_kb`.
fn sample(run: &mut TreeRun, stop_kb: u64) -> Result<bool> {
    let rss = proc::vm_rss_kb().context("read the RSS")?;
    run.peak_rss_kb = run.peak_rss_kb.max(rss);
    run.peak_threads = run
        .peak_threads
        .max(proc::threads().context("read the threads")?);
    Ok(rss > stop_kb)
}

/// The seed job on `root`: its layer, once it ran to its end.
fn seed_layer<A: Forks>(
    a: &A,
    s: &mut A::Session,
    root: &A::Layer,
    seed: &[Edit],
    scope: Scope,
) -> Result<A::Layer> {
    // The seed is cut off like every job of the bench: the series measures jobs under D1.
    let id = a
        .submit(s, root, seed, scope, Limit::Check)
        .context("submit the seed")?;
    loop {
        match a.next(s, NEXT_TICK).context("wait for the seed")? {
            Next::Done(got, Ok(checked)) if got == id => return Ok(checked.layer),
            Next::Done(got, Err(e)) if got == id => {
                return Err(e).context("the seed job failed");
            }
            Next::Withdrawn(got) if got == id => anyhow::bail!("the seed job was withdrawn"),
            Next::Pending => {}
            Next::Idle => anyhow::bail!("the seed job gave no outcome"),
            Next::Done(..) | Next::Expanded(..) | Next::Withdrawn(_) => {
                anyhow::bail!("an outcome of a job the bench did not submit")
            }
        }
    }
}

/// Submits the jobs of `level` (from 1) of `tree` on `parent`.
#[allow(clippy::too_many_arguments)]
fn submit_level<A: Forks>(
    a: &A,
    s: &mut A::Session,
    parent: &A::Layer,
    tree: &JobTree,
    level: usize,
    scope: Scope,
    inflight: &mut HashMap<JobId, Node>,
    run: &mut TreeRun,
) -> Result<()> {
    for job in tree.levels.get(level - 1).into_iter().flatten() {
        let id = a
            .submit(s, parent, &job.edits, scope, Limit::Check)
            .with_context(|| format!("submit job {}.{}", job.level, job.index))?;
        inflight.insert(id, (job.level, job.index));
        run.submitted += 1;
    }
    Ok(())
}

/// The tree on the open forks of `s`, from `root`: the seed, then each level as soon as its
/// parent is sealed. Every layer it holds is dropped before it returns.
fn drive<A: Forks>(
    a: &A,
    s: &mut A::Session,
    root: &A::Layer,
    tree: &JobTree,
    scope: Scope,
    observer: &RecordingObserver,
    stop_kb: u64,
) -> Result<TreeRun> {
    let mut run = TreeRun {
        width: a.width(s),
        ..TreeRun::default()
    };
    let t0 = Instant::now();
    let seed = seed_layer(a, s, root, &tree.seed, scope)?;
    run.seed_ms = t0.elapsed().as_secs_f64() * 1000.0;
    // The seed's events are not the tree's.
    observer.take();
    let cpu0 = proc::cpu_ms().context("read the CPU time")?;
    let started = Instant::now();
    // chain[k] is the parent of level k + 1.
    let mut chain: Vec<A::Layer> = vec![seed];
    let mut inflight: HashMap<JobId, Node> = HashMap::new();
    submit_level(a, s, &chain[0], tree, 1, scope, &mut inflight, &mut run)?;
    loop {
        match a.next(s, NEXT_TICK).context("wait for a job")? {
            Next::Done(id, result) => {
                let node = inflight
                    .remove(&id)
                    .context("an outcome of a job the bench did not submit")?;
                run.outcomes
                    .insert(node, JobOutcome::of(result.as_ref().map(|c| &c.findings)));
                match result {
                    Ok(checked) => {
                        run.done += 1;
                        let (level, index) = node;
                        if index == 0 && level < tree.levels.len() && !run.rss_stopped {
                            chain.push(checked.layer);
                            if let Some(parent) = chain.last() {
                                submit_level(
                                    a,
                                    s,
                                    parent,
                                    tree,
                                    level + 1,
                                    scope,
                                    &mut inflight,
                                    &mut run,
                                )?;
                            }
                        }
                        // Any other layer is dropped here: only the chain is kept.
                    }
                    Err(e) => {
                        run.failed += 1;
                        if let CheckError::TimedOut { limit_ms } = e {
                            run.timed_out += 1;
                            run.cut_ms.push(limit_ms);
                        }
                    }
                }
            }
            Next::Expanded(..) => anyhow::bail!("an expansion the bench did not ask for"),
            Next::Withdrawn(id) => {
                let node = inflight
                    .remove(&id)
                    .context("a withdrawal of a job the bench did not submit")?;
                run.outcomes.insert(node, JobOutcome::Withdrawn);
                run.withdrawn += 1;
            }
            Next::Pending => {}
            Next::Idle => break,
        }
        if sample(&mut run, stop_kb)? && !run.rss_stopped {
            run.rss_stopped = true;
            eprintln!(
                "[forks] RSS above the stop ({stop_kb} kB): withdrawing {} job(s)",
                inflight.len()
            );
            let ids: Vec<JobId> = inflight.keys().copied().collect();
            for id in ids {
                a.cancel(s, id).context("withdraw a job at the RSS stop")?;
            }
        }
    }
    run.wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    run.cpu_ms = proc::cpu_ms()
        .context("read the CPU time")?
        .saturating_sub(cpu0);
    run.checks = observer
        .take()
        .into_iter()
        .filter_map(|e| match e {
            Event::Checked {
                scope_files,
                check_ms,
                ..
            } => Some((scope_files, check_ms)),
            _ => None,
        })
        .collect();
    drop(chain);
    Ok(run)
}

/// One run of `tree` on forks of `s` opened by `a`: opens them, drives the tree, drops every
/// layer and closes them, whatever happened; a failed close after a failed run is printed.
pub fn run_tree<A: Forks>(
    a: &A,
    s: &mut A::Session,
    tree: &JobTree,
    scope: Scope,
    observer: &RecordingObserver,
    stop_kb: u64,
) -> Result<TreeRun> {
    let root = a.open(s).context("open the forks")?;
    let ran = drive(a, s, &root, tree, scope, observer, stop_kb);
    drop(root);
    let closed = a.close(s);
    match (ran, closed) {
        (Ok(run), Ok(())) => Ok(run),
        (Ok(_), Err(e)) => Err(e).context("close the forks"),
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(c)) => {
            eprintln!("[forks] close after a failed run: {c}");
            Err(e)
        }
    }
}

/// What the tree run serially, one job at a time at W = 1 ([`serial_tree`]), measured: each
/// job's check and outcome.
#[derive(Debug, Clone, Default)]
pub struct SerialRun {
    /// From the first write of level 1 to the last diagnosis.
    pub wall_ms: f64,
    /// Each job's `Event::Checked`: (scope files, check ms).
    pub checks: Vec<(usize, u64)>,
    /// The cut-off of each job cut off, in ms: a lower bound of its check time.
    pub cut_ms: Vec<u64>,
    /// Each job's outcome.
    pub outcomes: BTreeMap<Node, JobOutcome>,
}

impl SerialRun {
    /// The run as W4 sums it: every job it wrote counts as submitted; it has no RSS stop.
    pub fn side(&self) -> Side<'_> {
        Side {
            outcomes: &self.outcomes,
            rss_stopped: false,
            submitted: self.outcomes.len(),
        }
    }

    /// The check time of every job that ended or was cut off, a cut-off job at its cut-off.
    pub fn check_times(&self) -> impl Iterator<Item = u64> + '_ {
        self.checks
            .iter()
            .map(|&(_, ms)| ms)
            .chain(self.cut_ms.iter().copied())
    }
}

/// The tree serially, one job at a time on `Forks` at W = 1 (the analyzer's forks
/// [`SERIAL_FORKS`]): the seed as level 1, then per level its jobs 1.. each submitted on the
/// level's parent, waited for and its Layer dropped, and job 0 submitted last and its Layer kept
/// as the parent of the next level. Every Layer is dropped and the forks are closed at the end,
/// whatever happened.
pub fn serial_tree<A: Forks>(
    a: &A,
    s: &mut A::Session,
    tree: &JobTree,
    scope: Scope,
    observer: &RecordingObserver,
) -> Result<SerialRun> {
    let root = a.open(s).context("open the forks")?;
    let ran = serial_levels(a, s, &root, tree, scope, observer);
    drop(root);
    let closed = a.close(s);
    match (ran, closed) {
        (Ok(run), Ok(())) => Ok(run),
        (Ok(_), Err(e)) => Err(e).context("close the forks"),
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(c)) => {
            eprintln!("[forks] close after a failed serial run: {c}");
            Err(e)
        }
    }
}

/// The forks of the serial run: one job in flight on all T threads, as a level of the stacked
/// fork diagnosed on.
pub const SERIAL_FORKS: WidthSpec = WidthSpec {
    n: NonZeroUsize::MIN,
    caller_works: false,
};

/// The levels of [`serial_tree`] on the open forks, from `root`; every Layer they hold is dropped
/// when they return.
fn serial_levels<A: Forks>(
    a: &A,
    s: &mut A::Session,
    root: &A::Layer,
    tree: &JobTree,
    scope: Scope,
    observer: &RecordingObserver,
) -> Result<SerialRun> {
    let mut parent = seed_layer(a, s, root, &tree.seed, scope)?;
    observer.take();
    let mut run = SerialRun::default();
    let started = Instant::now();
    for level in &tree.levels {
        let Some((first, siblings)) = level.split_first() else {
            continue;
        };
        for job in siblings {
            let node = (job.level, job.index);
            let id = a
                .submit(s, &parent, &job.edits, scope, Limit::Check)
                .with_context(|| format!("submit job {}.{}", job.level, job.index))?;
            let found = crate::stacked::outcome(a, s, id)
                .with_context(|| format!("job {}.{}", job.level, job.index))?;
            if let Err(CheckError::TimedOut { limit_ms }) = &found {
                run.cut_ms.push(*limit_ms);
            }
            run.outcomes
                .insert(node, JobOutcome::of(found.as_ref().map(|c| &c.findings)));
            // The sibling's Layer is dropped here.
        }
        let node = (first.level, first.index);
        let id = a
            .submit(s, &parent, &first.edits, scope, Limit::Check)
            .with_context(|| format!("submit job {}.0", first.level))?;
        let found =
            crate::stacked::outcome(a, s, id).with_context(|| format!("job {}.0", first.level))?;
        if let Err(e @ CheckError::Edit { .. }) = &found {
            return Err(e.clone()).with_context(|| format!("write job {}.0", first.level));
        }
        if let Err(CheckError::TimedOut { limit_ms }) = &found {
            run.cut_ms.push(*limit_ms);
        }
        run.outcomes
            .insert(node, JobOutcome::of(found.as_ref().map(|c| &c.findings)));
        match found {
            Ok(checked) => parent = checked.layer,
            Err(_) => break,
        }
    }
    run.wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    run.checks = observer
        .take()
        .into_iter()
        .filter_map(|e| match e {
            Event::Checked {
                scope_files,
                check_ms,
                ..
            } => Some((scope_files, check_ms)),
            _ => None,
        })
        .collect();
    Ok(run)
}

/// Arguments of one `bench --forks` invocation.
pub struct ForksArgs {
    /// The workspace to load.
    pub base: PathBuf,
    /// The widths, in order.
    pub widths: Vec<WidthSpec>,
    /// The levels of the tree.
    pub depth: usize,
    /// The sibling jobs of each level.
    pub jobs: usize,
    /// The number of workspace files (in path order) the jobs rotate over, when `paths` is empty.
    pub files: u32,
    /// The workspace files the jobs rotate over, in this order.
    pub paths: Vec<RelPath>,
    /// Scopes to diagnose, in order.
    pub scopes: Vec<Scope>,
    /// Runs of the tree per width.
    pub repeat: u32,
    /// `RaConfig.threads`, T [default: the available parallelism].
    pub threads: Option<usize>,
    /// The cut-off of a job's check (D1); `None`: off, as W4 runs.
    pub cut_off_ms: Option<u64>,
    /// The RAM, in GiB, the run leaves free: it stops above the machine's RAM less this.
    pub rss_reserve_gib: f64,
    /// Whether to start the proc-macro server.
    pub proc_macro: bool,
    /// Whether to run the tree serially too, one job at a time at W = 1 ([`serial_tree`]), per
    /// repeat.
    pub serial: bool,
}

/// The configuration of the analyzer of one width: the load's, with `ForksConfig` of `spec`.
fn config(args: &ForksArgs, spec: Option<WidthSpec>) -> RaConfig {
    RaConfig {
        proc_macro: args.proc_macro,
        threads: args.threads,
        forks: spec.map_or_else(ForksConfig::default, |w| ForksConfig {
            width: Some(w.n),
            caller_works: w.caller_works,
        }),
        ..RaConfig::default()
    }
}

/// Loads `args.base` on the Engine once and, per scope, repeat and width, runs the job tree
/// through `Forks` ([`run_tree`]); prints a line per run, a summary per width
/// ([`summary_line`]) and the W4 verdict. A difference of W4, of a width or of the serial run
/// ([`serial_tree`]), is the run's error, after every line is printed. The serial run's summary
/// keeps its name, `serial-stacked`, from when it ran on one stacked fork.
pub fn run_forks(args: &ForksArgs) -> Result<()> {
    anyhow::ensure!(!args.widths.is_empty(), "--forks: no width");
    let cores = std::thread::available_parallelism()
        .context("the available parallelism")?
        .get();
    let mem_total_kb = proc::mem_total_kb().context("read the RAM")?;
    let stop_kb = rss_stop_kb(mem_total_kb, args.rss_reserve_gib);
    // The analyzer of each width is the loader's configuration with its `ForksConfig` and the
    // cut-off, the two things `Forks::open` reads; they share the loader's observer.
    let (loader, observer, mut session) =
        crate::bench::load_on_threads(&args.base, args.proc_macro, false, args.threads)?;
    let clock = Arc::new(MonotonicClock::new());
    let threads = session.threads();
    let rss_load = proc::vm_rss_kb().context("read the RSS after load")?;
    let files = if args.paths.is_empty() {
        crate::bench::depth_files(&loader, &session, &args.base, args.files)?
    } else {
        args.paths
            .iter()
            .map(|p| {
                loader
                    .text(&session, p)
                    .map(|t| (p.clone(), t))
                    .with_context(|| format!("--fork-paths: {p} is not an indexed file"))
            })
            .collect::<Result<Vec<_>>>()?
    };
    let tree = job_tree(&files, args.depth, args.jobs);
    anyhow::ensure!(tree.jobs() > 0, "--forks: the tree has no job");
    let names: Vec<String> = files.iter().map(|(p, _)| p.to_string()).collect();
    eprintln!("[forks] jobs rotate over {}", names.join(", "));
    let widest = args
        .widths
        .iter()
        .map(|w| shape(*w, threads).0)
        .max()
        .unwrap_or(1);
    let bound = tree_bound_bytes(args.depth, widest);
    let free = (stop_kb.saturating_sub(rss_load)) * 1024;
    println!(
        "[forks] machine cores {cores} ram_kb {mem_total_kb} threads {threads} rss_kb_after_load \
         {rss_load} rss_stop_kb {stop_kb} tree depth {} jobs_per_level {} jobs {} files {} \
         bound_bytes {bound} (Table M4 worst, W {widest}) free_bytes {free} cut_off_ms {}",
        args.depth,
        args.jobs,
        tree.jobs(),
        files.len(),
        args.cut_off_ms
            .map_or("off".to_owned(), |ms| ms.to_string()),
    );
    anyhow::ensure!(
        bound <= free,
        "--forks: the tree's bound ({bound} bytes) does not fit under the RSS stop ({free} bytes \
         free); lower --fork-depth or the widths"
    );
    let analyzers: Vec<RaAnalyzer> = args
        .widths
        .iter()
        .map(|w| {
            RaAnalyzer::new(config(args, Some(*w)), clock.clone(), observer.clone())
                .with_check_timeout(args.cut_off_ms)
        })
        .collect();
    let serial = RaAnalyzer::new(
        config(args, Some(SERIAL_FORKS)),
        clock.clone(),
        observer.clone(),
    )
    .with_check_timeout(args.cut_off_ms);
    // W = 1, `1` or `1c` (both give W = 1), is the reference of W4 and the base of the
    // speedup; without it the first width is W4's reference and no speedup is given.
    let one_at = args.widths.iter().position(|w| w.n.get() == 1);
    let reference_at = one_at.unwrap_or(0);
    let jobs = tree.jobs();
    // W4 runs with the cut-off off: then a comparison that is not complete fails too.
    let strict = args.cut_off_ms.is_none();
    let skip_timed_out = !strict;
    let mut diffs = 0usize;
    let mut incomplete = 0usize;
    for &scope in &args.scopes {
        let sname = scope_name(scope);
        // An untimed run at the first width: the frees the load leaves and the first
        // proc-macro expansions land here, not in a measured width.
        let warm = run_tree(
            &analyzers[0],
            &mut session,
            &tree,
            scope,
            &observer,
            stop_kb,
        )
        .context("the warm-up run")?;
        eprintln!(
            "[forks] warm-up [{sname}] width {}: wall_ms {:.0}",
            args.widths[0].label(),
            warm.wall_ms
        );
        // The serial run has its own untimed warm-up.
        if args.serial {
            let warm = serial_tree(&serial, &mut session, &tree, scope, &observer)
                .context("the serial warm-up run")?;
            eprintln!(
                "[forks] warm-up [{sname}] serial: wall_ms {:.0}",
                warm.wall_ms
            );
        }
        let mut runs: Vec<Vec<TreeRun>> = vec![Vec::new(); args.widths.len()];
        let mut serial_runs: Vec<SerialRun> = Vec::new();
        for r in 1..=args.repeat as usize {
            if args.serial {
                let run = serial_tree(&serial, &mut session, &tree, scope, &observer)
                    .context("the serial run")?;
                println!(
                    "serial [{sname}] {r}/{}: submitted {}/{jobs} wall_ms {:.0} jobs_per_s {:.2}",
                    args.repeat,
                    run.outcomes.len(),
                    run.wall_ms,
                    run.outcomes.len() as f64 / (run.wall_ms / 1000.0),
                );
                serial_runs.push(run);
            }
            // Repeat r starts at width (r − 1) mod n, so no width always runs first or last.
            for i in width_order(args.widths.len(), r - 1) {
                let spec = &args.widths[i];
                let a = &analyzers[i];
                let run = run_tree(a, &mut session, &tree, scope, &observer, stop_kb)
                    .with_context(|| format!("width {}", spec.label()))?;
                let (w, _) = shape(*spec, threads);
                anyhow::ensure!(
                    run.width == w,
                    "width {}: Forks::width is {} where the budget gives {w}",
                    spec.label(),
                    run.width
                );
                println!(
                    "{}",
                    run_line(sname, *spec, r, args.repeat as usize, jobs, &run)
                );
                runs[i].push(run);
            }
        }
        let ref_label = args.widths[reference_at].label();
        let empty = BTreeMap::new();
        let reference = runs[reference_at].first().map_or(
            Side {
                outcomes: &empty,
                rss_stopped: false,
                submitted: 0,
            },
            TreeRun::side,
        );
        // The reference run itself, whether or not anything is compared with it.
        if strict && (reference.rss_stopped || reference.submitted < jobs) {
            incomplete += 1;
            eprintln!(
                "[forks] W4 [{sname}] reference width {ref_label}: incomplete (rss_stop {}, \
                 submitted {}/{jobs})",
                reference.rss_stopped, reference.submitted
            );
        }
        let base_rate = one_at
            .map(|i| median_or_zero(&runs[i].iter().map(TreeRun::jobs_per_s).collect::<Vec<_>>()));
        let mut verdict = |label: &str, w: &W4| {
            if !w.differ.is_empty() {
                diffs += w.differ.len();
                eprintln!(
                    "[forks] W4 [{sname}] {label}: outcomes differ from width {ref_label} at {:?}",
                    w.differ
                );
            } else if w.fails(strict) {
                incomplete += 1;
                eprintln!(
                    "[forks] W4 [{sname}] {label}: incomplete against width {ref_label} (rows \
                     {}, left_out {}, stopped {}, short {})",
                    w.compared, w.left_out, w.stopped, w.short
                );
            }
        };
        for (i, spec) in args.widths.iter().enumerate() {
            // The reference width's repeats after its first are compared with its first.
            let compared: Vec<Side<'_>> = if i == reference_at {
                runs[i].iter().skip(1).map(TreeRun::side).collect()
            } else {
                runs[i].iter().map(TreeRun::side).collect()
            };
            let w4 =
                (!compared.is_empty()).then(|| w4_sum(reference, &compared, jobs, skip_timed_out));
            if let Some(w) = &w4 {
                verdict(&format!("width {}", spec.label()), w);
            }
            let summary = WidthSummary::of(
                scope, *spec, threads, cores, jobs, &runs[i], base_rate, w4, strict,
            );
            println!("{}", summary_line(&summary));
        }
        if args.serial {
            let sides: Vec<Side<'_>> = serial_runs.iter().map(SerialRun::side).collect();
            let sum = w4_sum(reference, &sides, jobs, skip_timed_out);
            let checks: Vec<i64> = serial_runs
                .iter()
                .flat_map(SerialRun::check_times)
                .map(|ms| ms as i64)
                .collect();
            let cut: usize = serial_runs.iter().map(|r| r.cut_ms.len()).sum();
            let rates: Vec<f64> = serial_runs
                .iter()
                .map(|r| r.outcomes.len() as f64 / (r.wall_ms / 1000.0))
                .collect();
            println!(
                "summary serial-stacked [{sname}] n={} jobs {jobs} jobs_per_s {:.2} check_ms {} \
                 cut_at_limit {cut} vs_width {ref_label} w4 {} rows {} diffs {} left_out {} \
                 stopped {} short {}",
                serial_runs.len(),
                median_or_zero(&rates),
                quantiles_txt(&Quantiles::of(&checks)),
                sum.verdict(strict),
                sum.compared,
                sum.differ.len(),
                sum.left_out,
                sum.stopped,
                sum.short,
            );
            if !sides.is_empty() {
                verdict("serial", &sum);
            }
        }
    }
    anyhow::ensure!(
        diffs == 0 && incomplete == 0,
        "W4: {diffs} job outcome(s) differ from width {}, {incomplete} comparison(s) incomplete \
         with the cut-off off",
        args.widths[reference_at].label()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn rel(p: &str) -> RelPath {
        RelPath::parse(p).unwrap()
    }

    fn spec(text: &str) -> WidthSpec {
        WidthSpec::parse(text).unwrap()
    }

    #[test]
    fn widths_parse_with_the_caller_suffix() {
        assert_eq!(
            spec("4"),
            WidthSpec {
                n: NonZeroUsize::new(4).unwrap(),
                caller_works: false
            }
        );
        assert_eq!(
            spec("11c"),
            WidthSpec {
                n: NonZeroUsize::new(11).unwrap(),
                caller_works: true
            }
        );
        assert_eq!(spec("11c").label(), "11c");
        assert_eq!(spec("16").label(), "16");
        for bad in ["0", "0c", "", "c", "-1", "+4", " 4", "4x", "4cc", "x"] {
            let e = WidthSpec::parse(bad).unwrap_err();
            assert!(e.contains(&format!("{bad:?}")), "{bad}: {e}");
        }
    }

    #[test]
    fn the_shape_follows_the_budget() {
        let at12 = |t: &str| shape(spec(t), 12);
        assert_eq!(at12("1"), (1, 12));
        assert_eq!(at12("4"), (4, 3));
        assert_eq!(at12("8"), (8, 1));
        assert_eq!(at12("12"), (12, 1));
        assert_eq!(at12("16"), (12, 1));
        assert_eq!(at12("11c"), (11, 1));
        assert_eq!(at12("12c"), (11, 1));
        assert_eq!(at12("4c"), (4, 2));
        // Oversubscribed: 16 threads on the session.
        assert_eq!(shape(spec("16"), 16), (16, 1));
        // The budget for every N and T: W ≤ T_w, t ≥ 1, W · t ≤ T_w.
        for t in 1..=32 {
            for n in 1..=32 {
                for c in [false, true] {
                    let s = WidthSpec {
                        n: NonZeroUsize::new(n).unwrap(),
                        caller_works: c,
                    };
                    let (w, jt) = shape(s, t);
                    let tw = (t - usize::from(c)).max(1);
                    assert!(w >= 1 && w <= tw && jt >= 1 && w * jt <= tw, "{n} {t} {c}");
                }
            }
        }
    }

    #[test]
    fn the_tree_writes_one_change_on_its_parent() {
        let files = [
            (rel("a.rs"), "fn a() {}".to_owned()),
            (rel("b.rs"), String::new()),
        ];
        let tree = job_tree(&files, 3, 5);
        assert_eq!(tree.levels.len(), 3);
        assert_eq!(tree.jobs(), 15);
        // The seed: one write per file, the body function returning 0 appended.
        assert_eq!(tree.seed.len(), 2);
        let seed_a = tree.seed[0].new_text.clone().unwrap();
        assert_eq!(seed_a, format!("fn a() {{}}\n{}", body_fn(0)));
        assert_eq!(tree.seed[1].new_text.clone().unwrap(), body_fn(0));
        // The texts at the parent of each level: the seed's, then job 0 of each level below.
        let mut at: BTreeMap<RelPath, String> = tree
            .seed
            .iter()
            .map(|e| (e.path.clone(), e.new_text.clone().unwrap()))
            .collect();
        let mut seen = BTreeSet::new();
        for (k, level) in tree.levels.iter().enumerate() {
            assert_eq!(level.len(), 5);
            for (j, job) in level.iter().enumerate() {
                assert_eq!((job.level, job.index), (k + 1, j));
                assert_eq!(job.edits.len(), 1, "one write per job");
                let edit = &job.edits[0];
                // The file rotates with the job's place in the tree.
                assert_eq!(edit.path, files[(k * 5 + j) % 2].0);
                // The class alternates every `files.len()` siblings.
                let want = if (j / 2) % 2 == 1 {
                    EditClass::Body
                } else {
                    EditClass::Item
                };
                assert_eq!(job.class, want, "{k}.{j}");
                let parent = &at[&edit.path];
                let text = edit.new_text.clone().unwrap();
                assert_ne!(&text, parent, "{k}.{j} repeats its parent");
                match job.class {
                    EditClass::Item => {
                        assert!(text.starts_with(parent.as_str()), "{k}.{j}");
                        assert!(text.contains(&format!("fn __ryo_fork_{}_{j}()", k + 1)));
                    }
                    EditClass::Body => {
                        // Same length class of change: only the literal of the body function.
                        let strip = |t: &str| {
                            let at = t.find(&format!("fn {FORK_BODY_FN}()")).unwrap();
                            (t[..at].to_owned(), t[at..].lines().count())
                        };
                        assert_eq!(strip(&text), strip(parent), "{k}.{j}");
                    }
                }
                assert!(seen.insert(text), "{k}.{j} writes a text another job wrote");
            }
            let first = &level[0].edits[0];
            at.insert(first.path.clone(), first.new_text.clone().unwrap());
        }
        // On one file the body job of level 2 changes the seed's literal on top of the chain's
        // item of level 1.
        let one = job_tree(&files[..1], 3, 2);
        let l2 = &one.levels[1];
        assert_eq!(l2[1].class, EditClass::Body);
        let text = l2[1].edits[0].new_text.as_deref().unwrap();
        assert!(
            text.contains(&body_fn(4)) && !text.contains(&body_fn(0)),
            "{text}"
        );
        assert!(text.contains("fn __ryo_fork_1_0()"), "{text}");
        assert!(job_tree(&[], 3, 2).levels.is_empty());
        assert_eq!(job_tree(&files, 0, 2).jobs(), 0);
        assert_eq!(job_tree(&files, 2, 0).jobs(), 0);
    }

    #[test]
    fn the_bound_counts_the_chain_and_the_jobs_in_flight() {
        assert_eq!(
            tree_bound_bytes(4, 12),
            18 * LAYER_BYTES_WORST + 12 * JOB_TRANSIENT_BYTES_WORST
        );
        assert_eq!(
            tree_bound_bytes(0, 1),
            3 * LAYER_BYTES_WORST + JOB_TRANSIENT_BYTES_WORST
        );
        // Depth 20 at width 12 is about 1.85 GB, well under the 17.36 GiB of Table M4's A.
        assert!(tree_bound_bytes(20, 12) < 2_000_000_000);
    }

    #[test]
    fn the_rss_stop_is_ram_less_the_reserve() {
        assert_eq!(rss_stop_kb(32 << 20, 8.0), 24 << 20);
        assert_eq!(rss_stop_kb(32 << 20, 0.5), (32 << 20) - (1 << 19));
        assert_eq!(rss_stop_kb(4 << 20, 8.0), 0);
        assert_eq!(rss_stop_kb(4 << 20, -1.0), 4 << 20);
    }

    fn done(files: &[&str]) -> JobOutcome {
        JobOutcome::Done(ScopedFindings {
            files: files.iter().map(|f| rel(f)).collect(),
            ..ScopedFindings::default()
        })
    }

    #[test]
    fn w4_compares_by_node_and_leaves_out_withdrawals() {
        let timed = JobOutcome::of(Err(&CheckError::TimedOut { limit_ms: 5 }));
        assert!(timed.timed_out());
        let edit = JobOutcome::of(Err(&CheckError::Edit {
            path: rel("a.rs"),
            reason: "one message".to_owned(),
        }));
        let edit_other_message = JobOutcome::of(Err(&CheckError::Edit {
            path: rel("a.rs"),
            reason: "another".to_owned(),
        }));
        // A failure is compared by its kind and files, not its message.
        assert_eq!(edit, edit_other_message);
        let reference = BTreeMap::from([
            ((1, 0), done(&["a.rs"])),
            ((1, 1), done(&["b.rs"])),
            ((1, 2), edit.clone()),
            ((1, 3), timed.clone()),
            ((1, 4), done(&["c.rs"])),
            ((2, 0), done(&["a.rs"])),
        ]);
        let run = BTreeMap::from([
            ((1, 0), done(&["a.rs"])),
            ((1, 1), done(&["b.rs", "c.rs"])),
            ((1, 2), edit_other_message),
            ((1, 3), done(&["d.rs"])),
            ((1, 4), JobOutcome::Withdrawn),
            ((3, 0), done(&["a.rs"])),
        ]);
        // Cut-off off: the cut-off row is compared and differs.
        let w4 = w4_diff(&reference, &run, false);
        assert_eq!(w4.differ, vec![(1, 1), (1, 3)]);
        assert_eq!(w4.compared, 4);
        // (1, 4) withdrawn, (2, 0) on the reference only, (3, 0) on the run only.
        assert_eq!(w4.left_out, 3);
        // Cut-off on: a row cut off on either side is left out.
        let w4 = w4_diff(&reference, &run, true);
        assert_eq!(w4.differ, vec![(1, 1)]);
        assert_eq!((w4.compared, w4.left_out), (3, 4));
        assert_eq!(
            w4_diff(&reference, &reference, false).differ,
            Vec::<Node>::new()
        );
    }

    #[test]
    fn w4_fails_when_incomplete_with_the_cut_off_off() {
        let full = BTreeMap::from([((1, 0), done(&["a.rs"])), ((1, 1), done(&["b.rs"]))]);
        let side = |outcomes, rss_stopped, submitted| Side {
            outcomes,
            rss_stopped,
            submitted,
        };
        let reference = side(&full, false, 2);
        // Two complete runs: compared and summed, equal under either rule.
        let w4 = w4_sum(
            reference,
            &[side(&full, false, 2), side(&full, false, 2)],
            2,
            false,
        );
        assert_eq!(
            (w4.compared, w4.left_out, w4.stopped, w4.short),
            (4, 0, 0, 0)
        );
        assert!(w4.complete() && !w4.fails(true) && !w4.fails(false));
        assert_eq!(w4.verdict(true), "equal");
        // A run with a node missing (its subtree never submitted) is short and leaves a row out.
        let partial = BTreeMap::from([((1, 0), done(&["a.rs"]))]);
        let w4 = w4_sum(reference, &[side(&partial, false, 1)], 2, false);
        assert_eq!((w4.compared, w4.left_out, w4.short), (1, 1, 1));
        assert!(w4.differ.is_empty() && w4.fails(true) && !w4.fails(false));
        assert_eq!(
            (w4.verdict(true), w4.verdict(false)),
            ("INCOMPLETE", "equal")
        );
        // Nodes missing on both sides alike: no row left out, but both runs are short.
        let w4 = w4_sum(
            side(&partial, false, 1),
            &[side(&partial, false, 1)],
            2,
            false,
        );
        assert_eq!((w4.compared, w4.left_out, w4.short), (1, 0, 2));
        assert!(w4.fails(true));
        // A run or the reference that met the RSS stop, with every row still compared.
        let w4 = w4_sum(reference, &[side(&full, true, 2)], 2, false);
        assert_eq!((w4.left_out, w4.stopped), (0, 1));
        assert!(w4.fails(true) && !w4.fails(false));
        let w4 = w4_sum(side(&full, true, 2), &[side(&full, false, 2)], 2, false);
        assert_eq!(w4.stopped, 1);
        assert!(w4.fails(true));
        // No row compared.
        let none = BTreeMap::new();
        let w4 = w4_sum(side(&none, false, 2), &[side(&none, false, 2)], 2, false);
        assert_eq!(w4.compared, 0);
        assert!(w4.fails(true) && !w4.fails(false));
        // No run: nothing summed, the reference not counted.
        assert_eq!(w4_sum(side(&full, true, 0), &[], 2, false), W4::default());
        // A difference fails under either rule.
        let other = BTreeMap::from([((1, 0), done(&["a.rs"])), ((1, 1), done(&["c.rs"]))]);
        let w4 = w4_sum(reference, &[side(&other, false, 2)], 2, false);
        assert_eq!(w4.differ, vec![(1, 1)]);
        assert!(w4.fails(true) && w4.fails(false));
        assert_eq!(w4.verdict(false), "DIFF");
    }

    #[test]
    fn the_width_order_rotates_per_repeat() {
        assert_eq!(width_order(3, 0), [0, 1, 2]);
        assert_eq!(width_order(3, 1), [1, 2, 0]);
        assert_eq!(width_order(3, 2), [2, 0, 1]);
        assert_eq!(width_order(3, 3), [0, 1, 2]);
        assert_eq!(width_order(1, 5), [0]);
        assert!(width_order(0, 2).is_empty());
        // Over n repeats every width runs once at each place.
        for n in 1..6 {
            for place in 0..n {
                let mut at: Vec<usize> = (0..n).map(|r| width_order(n, r)[place]).collect();
                at.sort_unstable();
                assert_eq!(at, (0..n).collect::<Vec<_>>());
            }
        }
    }

    #[test]
    fn the_summary_line_names_every_figure() {
        let run = |wall_ms: f64, checks: Vec<(usize, u64)>| TreeRun {
            width: 4,
            submitted: 4,
            done: 4,
            wall_ms,
            cpu_ms: 6000,
            checks,
            peak_rss_kb: 100,
            peak_threads: 30,
            ..TreeRun::default()
        };
        let mut cut = run(2000.0, vec![(2, 500)]);
        // A cut-off job counts at its cut-off in the check times, not in the scope splits.
        cut.cut_ms = vec![5000];
        cut.timed_out = 1;
        let runs = [
            run(1000.0, vec![(2, 400), (2, 600), (18, 900), (18, 6000)]),
            cut,
        ];
        assert_eq!(runs[0].jobs_per_s(), 4.0);
        assert_eq!(runs[0].busy_workers(), 7.9);
        assert_eq!(runs[1].check_times().collect::<Vec<_>>(), [500, 5000]);
        assert_eq!(runs[1].busy_workers(), 2.75);
        let s = WidthSummary::of(
            Scope::Affected,
            spec("4"),
            12,
            12,
            4,
            &runs,
            Some(1.0),
            Some(W4 {
                compared: 8,
                ..W4::default()
            }),
            true,
        );
        assert_eq!(
            (s.width, s.job_threads, s.capped, s.oversubscribed),
            (4, 3, false, false)
        );
        // The lower median of 4 and 2 jobs/s.
        assert_eq!(s.jobs_per_s, 2.0);
        assert_eq!(s.speedup, Some(2.0));
        assert_eq!(s.over_core4, 1);
        assert_eq!(
            summary_line(&s),
            "summary forks [affected] width 4 W 4 t 3 caller_works false capped false \
             oversubscribed false n=2 jobs 4 jobs_per_s 2.00 speedup 2.00 check_ms 6 \
             400/600/6000/6000 cut_at_limit 1 scope_lt8 3 400/500/600/600 scope_ge8 2 \
             900/900/6000/6000 busy_workers 2.75 occupancy 0.69 cpu_occupancy 0.25 peak_rss_kb \
             100 peak_threads 30 timed_out 1 over_5000ms 1 withdrawn 0 rss_stop false w4 equal \
             rows 8 diffs 0 left_out 0 stopped 0 short 0"
        );
        // The reference, a capped width, no checks, a difference.
        let capped = WidthSummary::of(
            Scope::Workspace,
            spec("16"),
            12,
            12,
            4,
            &[],
            None,
            None,
            true,
        );
        let line = summary_line(&capped);
        assert!(
            line.contains("width 16 W 12 t 1 caller_works false capped true"),
            "{line}"
        );
        assert!(
            line.contains("speedup - check_ms 0 - cut_at_limit 0 scope_lt8 0 - scope_ge8 0 -"),
            "{line}"
        );
        assert!(
            line.contains("w4 ref rows - diffs - left_out - stopped - short -"),
            "{line}"
        );
        // Incomplete: fails, and is named, only with the cut-off off.
        let short = W4 {
            compared: 3,
            short: 1,
            ..W4::default()
        };
        for (strict, want) in [(true, "INCOMPLETE"), (false, "equal")] {
            let s = WidthSummary::of(
                Scope::Affected,
                spec("4"),
                12,
                12,
                4,
                &runs,
                None,
                Some(short.clone()),
                strict,
            );
            let line = summary_line(&s);
            assert!(
                line.contains(&format!(
                    "w4 {want} rows 3 diffs 0 left_out 0 stopped 0 short 1"
                )),
                "{line}"
            );
        }
        let over = WidthSummary::of(
            Scope::Affected,
            spec("16"),
            16,
            12,
            4,
            &runs,
            None,
            Some(W4 {
                compared: 2,
                differ: vec![(1, 0)],
                left_out: 1,
                ..W4::default()
            }),
            false,
        );
        let line = summary_line(&over);
        assert!(line.contains("W 16 t 1 caller_works false capped false oversubscribed true"));
        assert!(line.contains("w4 DIFF rows 2 diffs 1 left_out 1"), "{line}");
    }
}
