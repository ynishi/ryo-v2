//! Ports the use cases need from the outside world.

use crate::domain::{
    Blocking, Blueprint, BlueprintId, ChangeProbe, CheckFailure, Edit, Finding, JudgeRequest,
    Judgement, Provenance, RelPath, Scope, ScopedFindings, StateFp, StaticMetrics, Target,
    UnseenClass,
};
use std::collections::BTreeSet;
use std::path::Path;

/// Failure of an [`Analyzer`] operation.
#[derive(Debug, thiserror::Error)]
pub enum AnalyzerError {
    /// The workspace could not be loaded.
    #[error("load failed: {0}")]
    Load(String),
    /// An edit could not be applied.
    #[error("edit rejected: {path}: {reason}")]
    Edit {
        /// The offending file.
        path: RelPath,
        /// Why.
        reason: String,
    },
    /// A fork is already live on the session.
    #[error("a fork is already live on this session")]
    ForkInUse,
    /// The fork already had edits applied.
    #[error("edits were already applied to this fork")]
    AlreadyApplied,
    /// The analysis was cancelled.
    #[error("analysis cancelled")]
    Cancelled,
    /// The analyzer panicked.
    #[error("rust-analyzer panicked: {0}")]
    Panicked(String),
    /// The session can no longer be used: an earlier panic or a failed revert poisoned it, and
    /// the operation was refused or the revert it ran could not be carried out or confirmed. The
    /// text names the cause. This is about the session, never about the edits of the operation.
    #[error("session poisoned: {0}")]
    Poisoned(String),
    /// The revert did not restore the texts, or (assert on) the diagnostics.
    #[error("revert did not restore the base state")]
    RevertMismatch,
}

pub use crate::domain::FileFacts;

/// Failure of a [`Verifier`] check.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CheckError {
    /// An edit could not be applied.
    #[error("edit rejected: {path}: {reason}")]
    Edit {
        /// The offending file.
        path: RelPath,
        /// Why.
        reason: String,
    },
    /// The analyzer cancelled the check by itself; nobody withdrew it. A withdrawal
    /// (`Forks::cancel`, the poison of a session) is `Next::Withdrawn`, never this.
    #[error("check cancelled")]
    Cancelled,
    /// The analyzer panicked.
    #[error("analyzer panicked: {0}")]
    Panicked(String),
    /// The session did not return to its base state after the check.
    #[error("revert did not restore the base state of: {}", join_paths(.paths))]
    RevertMismatch {
        /// The files whose text differs from before the check.
        paths: Vec<RelPath>,
    },
    /// The session can no longer be used.
    #[error("session poisoned: {0}")]
    Poisoned(String),
    /// The check was cut off after its time limit.
    #[error("check cut off after {limit_ms} ms")]
    TimedOut {
        /// The limit that was hit, in milliseconds.
        limit_ms: u64,
    },
    /// A fork is live on the session: the operation needs the session at its base state, or the
    /// ticket is not that of the live fork.
    #[error("a fork is live on the session, or the ticket is not that of the live fork")]
    ForkInUse,
    /// An `Analyzer` fork was written twice.
    #[error("edits were applied twice to one fork")]
    AlreadyApplied,
    /// The analyzer reported a load failure during a check.
    #[error("load failed: {0}")]
    Load(String),
    /// A detector panicked on a file the check read, so the check cannot judge that file. A check
    /// has restored the session; a diagnosis of a level leaves the levels as they were.
    #[error("detector {detector} panicked on {file}")]
    DetectorPanicked {
        /// The detector, by the adapter's name for it; the domain only shows it.
        detector: String,
        /// The file.
        file: RelPath,
    },
}

/// How far a [`CheckError`] reaches.
///
/// The adapter reverts the edits and compares the text fingerprints of the edited files after
/// every check, whatever its result, and a difference is `RevertMismatch`. A failure of reach
/// `Blueprint` leaves the session, and a fork's levels, as they were before the call: an `Edit`,
/// `Cancelled`, `Panicked` or `DetectorPanicked` that comes back says only this Blueprint's check
/// did not finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckReach {
    /// Only the Blueprint being checked: it is blocked, and the decision goes on.
    Blueprint,
    /// The session: the decision stops.
    Session,
}

impl CheckError {
    /// The reach of this failure.
    ///
    /// `RevertMismatch` and `Poisoned` are the poison of the session; `ForkInUse`: the caller
    /// holds a live fork while it asks the session for something else, or passes a ticket of
    /// another fork; `AlreadyApplied`: an `Analyzer` fork was written twice; `Load` means the
    /// session itself failed. None of these can be pinned on the Blueprint.
    ///
    /// # Contract
    /// - C1: `Edit`, `Cancelled`, `Panicked`, `DetectorPanicked` and `TimedOut` reach the
    ///   Blueprint; `RevertMismatch`, `Poisoned`, `ForkInUse`, `AlreadyApplied` and `Load` reach
    ///   the session. The match is exhaustive, so a new variant must be classified to compile.
    ///   tested: app::ports::tests::check_error_reach_is_decided_per_variant
    pub fn reach(&self) -> CheckReach {
        match self {
            CheckError::Edit { .. }
            | CheckError::Cancelled
            | CheckError::Panicked(_)
            | CheckError::DetectorPanicked { .. }
            | CheckError::TimedOut { .. } => CheckReach::Blueprint,
            CheckError::RevertMismatch { .. }
            | CheckError::Poisoned(_)
            | CheckError::ForkInUse
            | CheckError::AlreadyApplied
            | CheckError::Load(_) => CheckReach::Session,
        }
    }

    /// The blocking of `blueprint` for a failure of reach `Blueprint`; the failure itself for
    /// one of reach `Session`. `TimedOut` is `Blocking::BudgetExhausted`; `Edit`, `Cancelled`,
    /// `Panicked` and `DetectorPanicked` are `Blocking::CheckFailed`.
    pub fn into_blocking(self, blueprint: BlueprintId) -> Result<Blocking, CheckError> {
        match self {
            CheckError::TimedOut { limit_ms } => Ok(Blocking::BudgetExhausted {
                blueprint,
                limit_ms,
            }),
            other => other
                .into_failure()
                .map(|failure| Blocking::CheckFailed { blueprint, failure }),
        }
    }

    /// The `CheckFailure` of an `Edit`, `Cancelled`, `Panicked` or `DetectorPanicked`; the error
    /// itself for `TimedOut` (a cut-off is no failure of the check) and for a failure of reach
    /// `Session`.
    pub fn into_failure(self) -> Result<CheckFailure, CheckError> {
        match self {
            CheckError::Edit { path, reason } => Ok(CheckFailure::Edit { path, reason }),
            CheckError::Cancelled => Ok(CheckFailure::Cancelled),
            CheckError::Panicked(m) => Ok(CheckFailure::Panicked(m)),
            CheckError::DetectorPanicked { detector, file } => {
                Ok(CheckFailure::DetectorPanicked { detector, file })
            }
            other @ (CheckError::TimedOut { .. }
            | CheckError::RevertMismatch { .. }
            | CheckError::Poisoned(_)
            | CheckError::ForkInUse
            | CheckError::AlreadyApplied
            | CheckError::Load(_)) => Err(other),
        }
    }
}

fn join_paths(paths: &[RelPath]) -> String {
    paths
        .iter()
        .map(RelPath::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// What `apply_blueprints` and `prepare_write` need from the analyzer: the texts it holds, and a
/// check of edits that leaves no trace.
pub trait Verifier {
    /// A loaded workspace.
    type Session;
    /// The analyzer's current text of `path`; `None` if it is not a workspace file.
    fn text(&self, s: &Self::Session, path: &RelPath) -> Option<String>;
    /// What the workspace model says about `path`.
    fn facts(&self, s: &Self::Session, path: &RelPath) -> FileFacts;
    /// Every Error at load (all local files), message included.
    fn baseline(&self, s: &Self::Session) -> Vec<Finding>;
    /// The analyzer, its configuration and the fingerprint of each file of the session's base
    /// workspace. The domain never reads these values; it carries them into the evidence.
    fn provenance(&self, s: &Self::Session) -> Provenance;
    /// The classes of error this checker does not see, as it is configured. There is no default:
    /// an empty set is a claim that it sees every class, so each checker states its own.
    fn blind_classes(&self) -> BTreeSet<UnseenClass>;
    /// Applies `edits`, diagnoses the files of `scope`, and reverts. The observable state of
    /// the session afterwards is the state before.
    ///
    /// # Contract
    /// - C1: every finding of the result is on one of the result's `files`, one the analyzer
    ///   attributes to another file than the one it scanned included; `Verdict::of_check` and so
    ///   `app::verdict_of` and `Verdict::of_level` C3 rest on it.
    ///   tested: ryo-v2-ra tests/jobs.rs a_finding_is_on_a_file_of_its_diagnosis
    fn check(
        &self,
        s: &mut Self::Session,
        edits: &[Edit],
        scope: Scope,
    ) -> Result<ScopedFindings, CheckError>;
}

/// A [`Verifier`] that can also audit, on demand, that its checks left no trace.
pub trait RevertAudit: Verifier {
    /// Re-diagnoses the files edited since the last audit and compares them with the baseline;
    /// a difference poisons the session and is `CheckError::RevertMismatch`.
    fn audit_revert(&self, s: &mut Self::Session) -> Result<(), CheckError>;
}

/// Failure of an [`Expander`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExpandError {
    /// This parent yields nothing; the search goes on.
    #[error("expansion failed: {0}")]
    Analysis(String),
    /// The session is unusable; the search ends.
    #[error("session failure: {0}")]
    Session(CheckError),
}

/// Proposes the children of a search state: Blueprints whose reads and splices are against the
/// text of that state.
pub trait Expander {
    /// A loaded workspace.
    type Session;
    /// Children of the state `state` (edits over the session base), at most `max`, within
    /// `targets`. Observable session state afterwards = before. Reads and splices are against
    /// the state's text. An expansion of a Layer of an open [`Forks`] session is
    /// `Forks::expand`.
    fn expand(
        &self,
        s: &mut Self::Session,
        state: &[Edit],
        targets: &[Target],
        max: usize,
    ) -> Result<Vec<Blueprint>, ExpandError>;
}

/// The identity of a job of a [`Forks`] session. The adapter numbers them, and the ids of one
/// session increase in submission order.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(u64);

impl JobId {
    /// The id the adapter numbered `n`.
    pub fn new(n: u64) -> JobId {
        JobId(n)
    }

    /// The id's number.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// A job of a [`Forks`] session that ran to its end: its layer, which later jobs may write on
/// top of, and its diagnosis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked<L> {
    /// The job's layer: the state of its parent with the job's edits written on top.
    pub layer: L,
    /// The diagnosis of the job's scope on that layer.
    pub findings: ScopedFindings,
}

/// What [`Forks::next`] gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next<L> {
    /// The job ran: its layer and diagnosis, or why its check could not finish. An error here
    /// reaches the job's Blueprint only.
    Done(JobId, Result<Checked<L>, CheckError>),
    /// A read job of [`Forks::expand`] ran: the children of its Layer's state, or why that
    /// parent yields nothing (`ExpandError::Analysis`) or the session cannot go on
    /// (`ExpandError::Session`).
    Expanded(JobId, Result<Vec<Blueprint>, ExpandError>),
    /// The caller withdrew the job (`Forks::cancel` returned true), or the session withdrew it
    /// when it was poisoned: no layer, and no fact about its Blueprint. (The jobs `Forks::close`
    /// withdraws are not delivered: `next` is refused after it.)
    Withdrawn(JobId),
    /// The timeout passed with jobs in flight.
    Pending,
    /// No job is in flight or undelivered; given at once.
    Idle,
}

/// The time limit of a job of [`Forks::submit`].
///
/// D1's per-candidate cut-off is for a candidate's check. The search's start state is no
/// candidate: its job runs to its end, however long it takes (Forks C11).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Limit {
    /// The adapter's check cut-off (D1), counted from the job's start on a worker.
    Check,
    /// No cut-off.
    None,
}

/// A [`Verifier`] whose forks run side by side: layers over the session's frozen base, any
/// number alive at once, each job checked on a pool of workers (D11, stage 3b).
///
/// `open` freezes the base and gives its root as a Layer. A job (`submit`) writes its edits on
/// top of a Layer, resolved against that Layer's state as a load of that state would resolve them
/// (C13), diagnoses its scope there under the rules of a check, and finishes as a new Layer. Writes
/// stack: a child is a job on its parent's Layer, never rebuilt from the base. `next` gives the
/// outcomes in the order the jobs finish. A job's parent is always a finished Layer, never one in
/// flight; siblings are jobs on one parent. `expand` queues a read job that proposes the children
/// of a Layer's state, written nowhere. `cancel` withdraws a job. `close` ends the pool and thaws
/// the base.
///
/// `open`, `submit`, `expand`, `next`, `cancel` and `close` take the session mutably: the session
/// belongs to one thread, while the jobs run on the workers. The reads take it shared.
///
/// A withdrawal is no fact about a Blueprint, so it is `Next::Withdrawn`, never a [`CheckError`];
/// a `CheckError::Cancelled` inside a `Done` means the check could not finish although nobody
/// withdrew it. The errors of a call reach the session: `ForkInUse` for a Layer of another
/// session, an id that is unknown or already delivered, forks that are not open, and the
/// operations refused below; `Poisoned` for a poisoned session.
///
/// While forks are open the reads of [`Verifier`] describe the base, never a Layer (C21).
///
/// # Contract
/// - C1: until `close`, every accepted `submit` or `expand` yields exactly one `Done`, `Expanded`
///   or `Withdrawn` with its id (`Done` for a `submit`, `Expanded` for an `expand`), ids
///   increasing in submission order; edits are resolved at submit, and a refused edit is a `Done`
///   with `Edit` and no layer, never an `Err` of `submit`.
///   tested: app::ports::tests::outcomes_and_withdrawal_hold_for_the_fake
///   tested: ryo-v2-ra tests/forks.rs a_refused_edit_is_a_done_without_a_layer
///   tested: ryo-v2-ra tests/forks.rs parallel_jobs_equal_serial_and_the_reference
/// - C2: the layer of a `Done` diagnoses as its flat composition: at `Scope::Workspace` its
///   findings are what `Verifier::check` gives for the edits that take the base to its state,
///   and at `Scope::Affected` its files are the job's scope with the findings that check gives on
///   them, whatever ran beside it.
///   tested: app::ports::tests::a_done_layer_diagnoses_as_its_flat_composition_for_the_fake
///   tested: ryo-v2-ra tests/forks.rs parallel_jobs_equal_serial_and_the_reference
///   tested: ryo-v2-ra tests/jobs.rs a_chain_of_jobs_diagnoses_as_its_flat_composition
/// - C3: `cancel` returning true makes the job's outcome `Withdrawn`, never a `Done`, and leaves
///   the parent and every other job as they were; a job that already finished is not withdrawn
///   (false), and its outcome is what it produced.
///   tested: app::ports::tests::outcomes_and_withdrawal_hold_for_the_fake
///   tested: ryo-v2-ra tests/forks.rs a_cancel_withdraws_only_its_job
///   tested: ryo-v2-engine tests/jobs.rs cancel_true_is_always_withdrawn
/// - C4: at most `width` jobs run at once, an expansion's read jobs among them; with T the
///   adapter's threads, `width` is at most `max(1, T - 1)` when the caller works between its
///   calls and at most T when it only waits in `next`, and a job's diagnosis uses that share
///   divided by `width`, rounded down, and at least one thread.
///   tested: ryo-v2-ra tests/forks.rs job_threads_times_jobs_fit_the_budget
///   tested: ryo-v2-engine tests/jobs.rs a_read_job_counts_in_the_width
/// - C5: a Layer whose base is not this session's is `ForkInUse` before anything else, the
///   poison included, and changes nothing.
///   tested: app::ports::tests::the_fake_refuses_what_forks_refuse
///   tested: ryo-v2-ra tests/forks.rs a_layer_of_another_session_is_refused
/// - C6: while forks are open, `Verifier::check` and `RevertAudit::audit_revert` are `ForkInUse`, `Expander::expand` is `ExpandError::Session` with `ForkInUse`, and a second
///   `open` is `ForkInUse`; none of them changes anything. An expansion of a Layer is
///   `Forks::expand` (C10).
///   tested: app::ports::tests::the_fake_refuses_what_forks_refuse
///   tested: ryo-v2-ra tests/forks.rs session_operations_refuse_while_forks_are_open
///   tested: ryo-v2-ra tests/forks.rs expand_refuses_while_forks_are_open
/// - C7: a write the frozen base refused poisons the session and withdraws every job not yet
///   delivered: `next` gives their `Withdrawn`, then `Poisoned`. The full isolation check of the
///   base runs only while no job runs, so under continuous load only at `close`.
///   tested: app::ports::tests::a_refused_base_write_poisons_the_fake
///   tested: ryo-v2-ra tests/forks.rs a_refused_base_write_poisons_and_withdraws
/// - C8: `close` withdraws every job, joins the workers and checks the isolation of the base;
///   with a Layer still held it is `ForkInUse`, the base stays frozen and only `layer_files`,
///   `layer_text`, the reads of `Verifier` and `close` are allowed, until a `close` after the
///   last Layer is dropped thaws the base. A caller that keeps a Layer keeps the base frozen.
///   tested: app::ports::tests::a_refused_close_can_be_retried_on_the_fake
///   tested: ryo-v2-ra tests/forks.rs a_refused_close_can_be_retried
///   tested: ryo-v2-ra tests/forks.rs isolation_holds_after_parallel_jobs
/// - C10: `expand` of a Layer gives, as `Next::Expanded`, what `Expander::expand` gives for the
///   Layer's state with no fork open (the same Blueprints, ids included, the files the state
///   created among those it reads), whatever ran beside it; it writes nothing and counts in the
///   width (C4).
///   tested: app::ports::tests::outcomes_and_withdrawal_hold_for_the_fake
///   tested: ryo-v2-ra tests/forks.rs an_expansion_of_a_layer_is_that_of_its_state
///   tested: ryo-v2-ra tests/forks.rs expansions_run_beside_jobs_and_change_no_job
///   tested: ryo-v2-ra tests/forks.rs an_expansion_sees_a_file_the_state_created
///   tested: ryo-v2-ra tests/forks.rs an_expansion_counts_in_the_width
/// - C11: a job submitted with `Limit::None` is not cut off: it runs to its end and its outcome
///   is what the same job gives with no cut-off configured; with `Limit::Check` the adapter's
///   cut-off (D1) applies from the job's start on a worker.
///   tested: ryo-v2-ra tests/forks.rs a_start_job_is_not_cut_off
/// - C12: with no forks open, `Verifier::check` of edits at a scope gives what a session with one
///   job gives for the same edits: `open`, a `submit` of them on the root Layer with
///   `Limit::Check` at that scope, its `Done` (the findings, or the error), and `close`; the texts
///   after either are the base's.
///   tested: ryo-v2-ra tests/jobs.rs check_is_a_one_job_session
/// - C13: a job's edits resolve against its parent's state: a path that is not a file of that
///   state is created, one an earlier job of the chain deleted included (revived); a path that is
///   a file of it is modified or deleted; deleting a path that is not a file of it is a `Done`
///   with `Edit` and no layer. The parent and every Layer below it keep their texts.
///   tested: ryo-v2-ra tests/jobs.rs a_job_writes_against_its_parents_state
/// - C14: a job's `Scope::Affected` covers the files of the crates its edits touch and of their
///   dependents, at its layer's state (files it created included, files it deleted excluded);
///   `Scope::Workspace` covers every file of that state.
///   tested: ryo-v2-ra tests/jobs.rs affected_scope_is_the_jobs
/// - C15: a file outside a job's `Scope::Affected` has, at its layer, the findings it has at its
///   parent's, and the same files under each class of `unseen_at`.
///   tested: ryo-v2-ra tests/jobs.rs files_outside_a_jobs_scope_keep_its_parents_findings
/// - C16: a job with no edit diagnoses no file at `Scope::Affected` and names no unseen class.
///   tested: ryo-v2-ra tests/jobs.rs an_empty_job_diagnoses_nothing
/// - C17: a job whose diagnosis the adapter's cut-off reaches is a `Done` with `TimedOut` and no
///   layer; its parent stays usable.
///   tested: ryo-v2-ra tests/jobs.rs a_cut_off_job_keeps_its_parent_usable
/// - C18: the unseen classes of a `Done` are those its diagnosis observed at its layer; a class
///   observed at a parent's and not there is not among them. Every class of its `unseen_at`, and
///   of a check's, names at least one file, and only files of the result.
///   tested: ryo-v2-ra tests/jobs.rs a_job_names_the_unseen_it_observes
///   tested: ryo-v2-ra tests/jobs.rs a_diagnosis_names_where_it_observed_each_unseen_class
/// - C19: a detector that panics on a file of a job's diagnosis, where it did not panic on that
///   file at the baseline, makes the `Done` `DetectorPanicked` (the first by detector and file),
///   whatever its parents had; one that panicked on that file at the baseline too names its class
///   among the unseen classes, whatever its parents had. Either way the parent stays usable.
///   tested: ryo-v2-ra tests/jobs.rs a_new_detector_panic_fails_the_job_and_keeps_its_parent
///   tested: ryo-v2-ra tests/jobs.rs a_baseline_panic_brought_back_by_a_job_is_named
///   tested: ryo-v2-ra tests/jobs.rs a_baseline_detector_panic_is_named_at_every_job
/// - C20: every finding of a `Done`, and of a check, is on one of the result's `files`, one the
///   analyzer attributes to another file than the one it scanned included. A parent's finding is
///   so on a file of its own diagnosis, and a file diagnosed again replaces it
///   (`ScopedFindings::on_top_of`).
///   tested: ryo-v2-ra tests/jobs.rs a_finding_is_on_a_file_of_its_diagnosis
/// - C21: while forks are open, `Verifier::text`, `Verifier::provenance` and
///   `Verifier::baseline` describe the base, never a Layer.
///   tested: ryo-v2-ra tests/jobs.rs verifier_reads_describe_the_base_while_forks_are_open
pub trait Forks: Verifier {
    /// A finished job's layer, or the base's root: what a job writes on top of. Holding one
    /// keeps the base frozen (C8).
    type Layer: Clone + Send;

    /// Freezes the session's base and gives its root. A session with forks open, or with another
    /// fork of the adapter live, is `ForkInUse`; a poisoned one is `Poisoned`.
    fn open(&self, s: &mut Self::Session) -> Result<Self::Layer, CheckError>;

    /// The jobs that run at once (C4).
    fn width(&self, s: &Self::Session) -> usize;

    /// Resolves `edits` against `parent`'s state and queues a job that writes them on top of it
    /// and diagnoses `scope` there (C1), within `limit` (C11).
    fn submit(
        &self,
        s: &mut Self::Session,
        parent: &Self::Layer,
        edits: &[Edit],
        scope: Scope,
        limit: Limit,
    ) -> Result<JobId, CheckError>;

    /// Queues a read job that proposes at most `max` children of `layer`'s state within
    /// `targets` (C10). `state` is the fingerprint of that state's edits over the base, the
    /// identity the Blueprints' ids are made of (`Expander` C8): the caller keeps it with the
    /// Layer, so no edit list has to be handed back.
    fn expand(
        &self,
        s: &mut Self::Session,
        layer: &Self::Layer,
        state: StateFp,
        targets: &[Target],
        max: usize,
    ) -> Result<JobId, CheckError>;

    /// The next outcome, waiting at most `timeout` for one while jobs are in flight. `timeout`
    /// bounds this wait only; a job's own time limit starts when a worker starts it.
    fn next(
        &self,
        s: &mut Self::Session,
        timeout: std::time::Duration,
    ) -> Result<Next<Self::Layer>, CheckError>;

    /// Withdraws `job` if it has not finished (C3). An id that is unknown or already delivered
    /// is `ForkInUse`.
    fn cancel(&self, s: &mut Self::Session, job: JobId) -> Result<bool, CheckError>;

    /// The files a `Verifier::check` at `scope` covers for the edits that take the base to
    /// `layer`'s state. The adapter derives those edits' paths from the jobs of the Layer's chain,
    /// so a caller cannot name a wrong set. The caller keeps a composed verdict to these files
    /// (`Verdict::of_level`).
    ///
    /// # Contract
    /// - C1: the files, sorted, are those of `Verifier::check` at `scope` of the edits that take
    ///   the base to `layer`'s state: one edit per path some job of the chain edited whose
    ///   presence or text at `layer` differs from the base, and no other.
    ///   tested: ryo-v2-ra tests/jobs.rs layer_files_and_of_level_equal_the_check_over_a_chain
    /// - C2: for the root, or with every path the chain edited back at its base presence and text,
    ///   the result at `Scope::Affected` is empty, as a check with no edit diagnoses no file there;
    ///   at `Scope::Workspace` it is every file of `layer`'s state, as such a check diagnoses every
    ///   file.
    ///   tested: ryo-v2-ra tests/jobs.rs layer_files_and_of_level_equal_the_check_over_a_chain
    fn layer_files(
        &self,
        s: &Self::Session,
        layer: &Self::Layer,
        scope: Scope,
    ) -> Result<Vec<RelPath>, CheckError>;

    /// The text of `path` at `layer`; `Ok(None)` when `path` is not a file of that state.
    fn layer_text(
        &self,
        s: &Self::Session,
        layer: &Self::Layer,
        path: &RelPath,
    ) -> Result<Option<String>, CheckError>;

    /// Ends the forks: withdraws every job, joins the pool, checks the base and thaws it (C8).
    fn close(&self, s: &mut Self::Session) -> Result<(), CheckError>;
}

/// Measures what a child changed, from the texts alone.
pub trait Assessor {
    /// `before[i]` and `after[i]` are the same path: the parent's and the child's whole text.
    fn assess(&self, before: &[Edit], after: &[Edit], probes: &[ChangeProbe]) -> StaticMetrics;
}

/// Failure of a [`Judge`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JudgeError {
    /// The provider could not answer.
    #[error("judge failed: {0}")]
    Provider(String),
}

/// Gives priors and objections for proposals that nothing has verified yet.
pub trait Judge {
    /// One judgement for the whole request.
    fn judge(&self, req: &JudgeRequest) -> Result<Judgement, JudgeError>;
}

/// 計測・進捗の event。時間は Clock::now_ms の差 (ms)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Workspace indexed: the load's files are known and the baseline has not started.
    ///
    /// # Contract
    /// - C1: an adapter that reports it emits it once per load, before `Event::Loaded`, with the
    ///   same `load_ms` and `files` as that `Event::Loaded`.
    ///   tested: ryo-v2-ra tests/smoke.rs indexed_comes_before_loaded
    Indexed {
        /// Load time, as in the `Event::Loaded` of the same load.
        load_ms: u64,
        /// Local files, as in the `Event::Loaded` of the same load.
        files: usize,
    },
    /// Workspace loaded and baseline taken.
    Loaded {
        /// Load time.
        load_ms: u64,
        /// Baseline diagnostics time.
        baseline_ms: u64,
        /// Local files.
        files: usize,
        /// Errors at load.
        baseline_errors: usize,
    },
    /// A diagnosis of a scope ran to its end (a check's or a level's); a cut-off or a failed
    /// collect records none.
    Checked {
        /// Files diagnosed.
        scope_files: usize,
        /// Local files at load.
        total_files: usize,
        /// Check time: the duration of the write of the diagnosed level plus the duration of this
        /// diagnosis. The time between the write and the diagnosis, and between two diagnoses of
        /// one level, is not counted, and neither is an earlier diagnosis.
        check_ms: u64,
    },
    /// A level of a fork was reverted (a discard, a pop, a level of a release, the one level of a
    /// check); a refused write pushed no level and records none, and a release on a poisoned
    /// session reverts nothing.
    Reverted {
        /// Revert time.
        revert_ms: u64,
        /// Whether the post-revert diagnostics matched those of the level below (the baseline
        /// for the first level); None = not asserted.
        revert_ok: Option<bool>,
    },
    /// Resident set size of the process, sampled by the caller after a load or a discard
    /// (the host reads `/proc/self/status`; the core never reads it).
    Memory {
        /// VmRSS in kB.
        rss_kb: u64,
    },
}

/// Monotonic time source.
pub trait Clock: Send + Sync {
    /// 単調増加の ms。原点は実装が決める (差だけに意味がある)。
    fn now_ms(&self) -> u64;
}

/// Reads the memory of this process, for the RSS stop of a search (`app::memory::RssStop`).
///
/// `None` means the figure cannot be read here (no `/proc`): a stop that cannot read never fires,
/// so a gauge never fails a search.
pub trait Gauge: Send + Sync {
    /// The resident set size of this process in kB.
    fn rss_kb(&self) -> Option<u64>;
}

/// Receives [`Event`]s.
pub trait Observer: Send + Sync {
    /// Called once per event.
    fn observe(&self, event: Event);
}

/// 1 Session = load 済み workspace 1 つ。Fork は Session の上の 1 つの編集集合で、同時に生きる Fork は 1 Session に 1 つ。
pub trait Analyzer {
    /// A loaded workspace.
    type Session;
    /// One edit set over a session.
    type Fork;
    /// workspace を load して baseline (load 時点の Error) まで取る。
    fn load(&self, root: &Path) -> Result<Self::Session, AnalyzerError>;
    /// Every Error at load (all local files), message included. How it is compared is
    /// `domain::baseline`, not the adapter's business.
    fn baseline(&self, session: &Self::Session) -> Vec<Finding>;
    /// 空の Fork を作る。既に生きた Fork があれば Err(ForkInUse)。
    fn fork(&self, session: &mut Self::Session) -> Result<Self::Fork, AnalyzerError>;
    /// 編集を session の in-memory state に適用する。1 Fork に 1 回だけ (2 回目は Err(AlreadyApplied))。
    fn apply(
        &self,
        session: &mut Self::Session,
        fork: &mut Self::Fork,
        edits: &[Edit],
    ) -> Result<(), AnalyzerError>;
    /// Every Error in the files of `scope` after the edits, message included, and the list of
    /// those files. No baseline difference here. Before apply: `ScopedFindings::default()`.
    fn diagnostics(
        &self,
        session: &Self::Session,
        fork: &Self::Fork,
        scope: Scope,
    ) -> Result<ScopedFindings, AnalyzerError>;
    /// 適用を巻き戻して Fork を捨てる。巻き戻しが text を戻さなかった時、または (assert_revert 有効時) 巻き戻し後の診断が baseline と違う時は Err(RevertMismatch)。
    fn discard(&self, session: &mut Self::Session, fork: Self::Fork) -> Result<(), AnalyzerError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ErrorCode, FindingCode};
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Arc;
    use std::time::Duration;

    fn rel(s: &str) -> RelPath {
        RelPath::parse(s).unwrap()
    }

    /// A fake [`Forks`] that keeps the port's clauses with no analyzer: a state is a map of
    /// texts, a file whose text contains `error` has one finding, and a job finishes at the
    /// first `next` after its submit, the newest delivered first (completion out of submission
    /// order). It is the test double of the port's consumers, and checks that the clauses fit
    /// together.
    struct FakeForks;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct FakeLayer {
        session: u64,
        texts: BTreeMap<RelPath, String>,
        /// A clone of the session's `held`: the Layers alive.
        held: Arc<()>,
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum Open {
        No,
        Yes,
        /// A `close` was refused because a Layer was held.
        Closing,
    }

    struct FakeSession {
        id: u64,
        base: BTreeMap<RelPath, String>,
        open: Open,
        poisoned: bool,
        next_id: u64,
        /// Accepted jobs not delivered, with the outcome they will have and whether they
        /// finished.
        in_flight: BTreeMap<JobId, (bool, Next<FakeLayer>)>,
        /// Outcomes known at once (refused edits, withdrawals), delivered first.
        ready: VecDeque<Next<FakeLayer>>,
        held: Arc<()>,
        /// The next `next` finds a write the frozen base refused.
        refused_write: bool,
    }

    impl FakeSession {
        fn new(id: u64, base: &[(&str, &str)]) -> FakeSession {
            FakeSession {
                id,
                base: base
                    .iter()
                    .map(|(p, t)| (rel(p), (*t).to_owned()))
                    .collect(),
                open: Open::No,
                poisoned: false,
                next_id: 0,
                in_flight: BTreeMap::new(),
                ready: VecDeque::new(),
                held: Arc::new(()),
                refused_write: false,
            }
        }

        fn layer(&self, texts: BTreeMap<RelPath, String>) -> FakeLayer {
            FakeLayer {
                session: self.id,
                texts,
                held: Arc::clone(&self.held),
            }
        }

        fn layers_held(&self) -> usize {
            Arc::strong_count(&self.held) - 1
        }

        fn refuse_while_open(&self) -> Result<(), CheckError> {
            match self.open {
                Open::No => Ok(()),
                Open::Yes | Open::Closing => Err(CheckError::ForkInUse),
            }
        }

        /// The id of a job on `parent`, when this session takes one (C5 before the poison).
        fn accept(&mut self, parent: &FakeLayer) -> Result<JobId, CheckError> {
            if parent.session != self.id || self.open != Open::Yes {
                return Err(CheckError::ForkInUse);
            }
            if self.poisoned {
                return Err(CheckError::Poisoned("fake".to_owned()));
            }
            let id = JobId::new(self.next_id);
            self.next_id += 1;
            Ok(id)
        }

        fn withdraw_all(&mut self) {
            for id in std::mem::take(&mut self.in_flight).into_keys() {
                self.ready.push_back(Next::Withdrawn(id));
            }
        }
    }

    /// `state` with `edits` written on top; deleting a path that is not a file of it is `Edit`.
    fn written(
        state: &BTreeMap<RelPath, String>,
        edits: &[Edit],
    ) -> Result<BTreeMap<RelPath, String>, CheckError> {
        let mut state = state.clone();
        for edit in edits {
            match &edit.new_text {
                Some(text) => {
                    state.insert(edit.path.clone(), text.clone());
                }
                None => {
                    if state.remove(&edit.path).is_none() {
                        return Err(CheckError::Edit {
                            path: edit.path.clone(),
                            reason: "not a file of this state".to_owned(),
                        });
                    }
                }
            }
        }
        Ok(state)
    }

    /// The fake's expansion of `state`: no Blueprint, and a parent that yields nothing when a
    /// target is not a file of the state (so a created file is told from a missing one).
    fn fake_children(
        state: &BTreeMap<RelPath, String>,
        targets: &[Target],
    ) -> Result<Vec<Blueprint>, ExpandError> {
        match targets.iter().find(|t| !state.contains_key(t.path())) {
            Some(t) => Err(ExpandError::Analysis(format!("no file {}", t.path()))),
            None => Ok(Vec::new()),
        }
    }

    /// The files of a diagnosis at `scope` of `state`, reached by `edits`.
    fn scope_files(
        state: &BTreeMap<RelPath, String>,
        edits: &[Edit],
        scope: Scope,
    ) -> Vec<RelPath> {
        match scope {
            Scope::Workspace => state.keys().cloned().collect(),
            Scope::Affected => {
                let mut files: Vec<RelPath> = edits
                    .iter()
                    .map(|e| e.path.clone())
                    .filter(|p| state.contains_key(p))
                    .collect();
                files.sort();
                files.dedup();
                files
            }
        }
    }

    fn diagnose(state: &BTreeMap<RelPath, String>, files: Vec<RelPath>) -> ScopedFindings {
        let findings = files
            .iter()
            .filter(|p| state[*p].contains("error"))
            .map(|p| Finding {
                code: FindingCode::Rustc(ErrorCode::parse("E0599").unwrap()),
                message: "fake".to_owned(),
                path: p.clone(),
                line: 1,
            })
            .collect();
        ScopedFindings {
            files,
            findings,
            unseen_at: BTreeMap::new(),
        }
    }

    /// The edits that take `base` to `state`.
    fn edits_to(base: &BTreeMap<RelPath, String>, state: &BTreeMap<RelPath, String>) -> Vec<Edit> {
        let mut edits: Vec<Edit> = state
            .iter()
            .filter(|(p, t)| base.get(*p) != Some(*t))
            .map(|(p, t)| Edit::write(p.clone(), t.clone()))
            .collect();
        edits.extend(
            base.keys()
                .filter(|p| !state.contains_key(*p))
                .map(|p| Edit {
                    path: p.clone(),
                    new_text: None,
                }),
        );
        edits.sort_by(|a, b| a.path.cmp(&b.path));
        edits
    }

    impl Verifier for FakeForks {
        type Session = FakeSession;
        fn text(&self, s: &FakeSession, path: &RelPath) -> Option<String> {
            s.base.get(path).cloned()
        }
        fn facts(&self, _: &FakeSession, _: &RelPath) -> FileFacts {
            FileFacts {
                is_build_script: false,
                in_proc_macro_crate: false,
            }
        }
        fn baseline(&self, _: &FakeSession) -> Vec<Finding> {
            Vec::new()
        }
        fn provenance(&self, _: &FakeSession) -> Provenance {
            Provenance::new("fake", "0", BTreeMap::new(), BTreeMap::new()).unwrap()
        }
        fn blind_classes(&self) -> BTreeSet<UnseenClass> {
            BTreeSet::from(crate::app::ALWAYS_BLIND)
        }
        fn check(
            &self,
            s: &mut FakeSession,
            edits: &[Edit],
            scope: Scope,
        ) -> Result<ScopedFindings, CheckError> {
            s.refuse_while_open()?;
            let state = written(&s.base, edits)?;
            Ok(diagnose(&state, scope_files(&state, edits, scope)))
        }
    }

    impl RevertAudit for FakeForks {
        fn audit_revert(&self, s: &mut FakeSession) -> Result<(), CheckError> {
            s.refuse_while_open()
        }
    }

    impl Expander for FakeForks {
        type Session = FakeSession;
        fn expand(
            &self,
            s: &mut FakeSession,
            state: &[Edit],
            targets: &[Target],
            _: usize,
        ) -> Result<Vec<Blueprint>, ExpandError> {
            s.refuse_while_open().map_err(ExpandError::Session)?;
            let state = written(&s.base, state)
                .map_err(|e| ExpandError::Analysis(format!("state edit rejected: {e}")))?;
            fake_children(&state, targets)
        }
    }

    impl Forks for FakeForks {
        type Layer = FakeLayer;

        fn open(&self, s: &mut FakeSession) -> Result<FakeLayer, CheckError> {
            s.refuse_while_open()?;
            if s.poisoned {
                return Err(CheckError::Poisoned("fake".to_owned()));
            }
            s.open = Open::Yes;
            Ok(s.layer(s.base.clone()))
        }

        fn width(&self, _: &FakeSession) -> usize {
            4
        }

        fn submit(
            &self,
            s: &mut FakeSession,
            parent: &FakeLayer,
            edits: &[Edit],
            scope: Scope,
            _: Limit,
        ) -> Result<JobId, CheckError> {
            let id = s.accept(parent)?;
            match written(&parent.texts, edits) {
                Err(refused) => s.ready.push_back(Next::Done(id, Err(refused))),
                Ok(state) => {
                    let findings = diagnose(&state, scope_files(&state, edits, scope));
                    let layer = s.layer(state);
                    s.in_flight
                        .insert(id, (false, Next::Done(id, Ok(Checked { layer, findings }))));
                }
            }
            Ok(id)
        }

        fn expand(
            &self,
            s: &mut FakeSession,
            layer: &FakeLayer,
            _: StateFp,
            targets: &[Target],
            _: usize,
        ) -> Result<JobId, CheckError> {
            let id = s.accept(layer)?;
            let children = fake_children(&layer.texts, targets);
            s.in_flight
                .insert(id, (false, Next::Expanded(id, children)));
            Ok(id)
        }

        fn next(&self, s: &mut FakeSession, _: Duration) -> Result<Next<FakeLayer>, CheckError> {
            if s.open != Open::Yes {
                return Err(CheckError::ForkInUse);
            }
            if std::mem::take(&mut s.refused_write) {
                s.poisoned = true;
                s.withdraw_all();
            }
            if let Some(next) = s.ready.pop_front() {
                return Ok(next);
            }
            if s.poisoned {
                return Err(CheckError::Poisoned(
                    "a write of the frozen base".to_owned(),
                ));
            }
            for (finished, _) in s.in_flight.values_mut() {
                *finished = true;
            }
            match s.in_flight.pop_last() {
                Some((_, (_, outcome))) => Ok(outcome),
                None => Ok(Next::Idle),
            }
        }

        fn cancel(&self, s: &mut FakeSession, job: JobId) -> Result<bool, CheckError> {
            if s.open != Open::Yes {
                return Err(CheckError::ForkInUse);
            }
            match s.in_flight.get(&job) {
                // Known and not delivered, but already finished (a refused edit) or withdrawn.
                None if s.ready.iter().any(|next| {
                    matches!(next,
                    Next::Done(id, _) | Next::Withdrawn(id) if *id == job)
                }) =>
                {
                    Ok(false)
                }
                None => Err(CheckError::ForkInUse),
                Some((true, _)) => Ok(false),
                Some((false, _)) => {
                    s.in_flight.remove(&job);
                    s.ready.push_back(Next::Withdrawn(job));
                    Ok(true)
                }
            }
        }

        fn layer_files(
            &self,
            s: &FakeSession,
            layer: &FakeLayer,
            scope: Scope,
        ) -> Result<Vec<RelPath>, CheckError> {
            if layer.session != s.id || s.open == Open::No {
                return Err(CheckError::ForkInUse);
            }
            let edits = edits_to(&s.base, &layer.texts);
            Ok(scope_files(&layer.texts, &edits, scope))
        }

        fn layer_text(
            &self,
            s: &FakeSession,
            layer: &FakeLayer,
            path: &RelPath,
        ) -> Result<Option<String>, CheckError> {
            if layer.session != s.id || s.open == Open::No {
                return Err(CheckError::ForkInUse);
            }
            Ok(layer.texts.get(path).cloned())
        }

        fn close(&self, s: &mut FakeSession) -> Result<(), CheckError> {
            if s.open == Open::No {
                return Err(CheckError::ForkInUse);
            }
            s.in_flight.clear();
            s.ready.clear();
            if s.layers_held() > 0 {
                s.open = Open::Closing;
                return Err(CheckError::ForkInUse);
            }
            s.open = Open::No;
            if s.poisoned {
                return Err(CheckError::Poisoned(
                    "a write of the frozen base".to_owned(),
                ));
            }
            Ok(())
        }
    }

    const FOREVER: Duration = Duration::from_secs(60);

    fn engine_session() -> (FakeForks, FakeSession) {
        (
            FakeForks,
            FakeSession::new(1, &[("src/a.rs", "a"), ("src/b.rs", "b")]),
        )
    }

    /// Every outcome until `Idle`.
    fn drain(f: &FakeForks, s: &mut FakeSession) -> Vec<Next<FakeLayer>> {
        let mut out = Vec::new();
        loop {
            match f.next(s, FOREVER).unwrap() {
                Next::Idle => return out,
                Next::Pending => panic!("the fake is never pending"),
                next => out.push(next),
            }
        }
    }

    fn id_of(next: &Next<FakeLayer>) -> JobId {
        match next {
            Next::Done(id, _) | Next::Expanded(id, _) | Next::Withdrawn(id) => *id,
            Next::Pending | Next::Idle => panic!("no job: {next:?}"),
        }
    }

    #[test]
    fn outcomes_and_withdrawal_hold_for_the_fake() {
        let (f, mut s) = engine_session();
        let root = f.open(&mut s).unwrap();
        assert_eq!(f.next(&mut s, FOREVER), Ok(Next::Idle), "Idle at once");

        let write = |text: &str| vec![Edit::write(rel("src/a.rs"), text)];
        let a = f
            .submit(&mut s, &root, &write("a1"), Scope::Affected, Limit::Check)
            .unwrap();
        let refused = f
            .submit(
                &mut s,
                &root,
                &[Edit {
                    path: rel("src/none.rs"),
                    new_text: None,
                }],
                Scope::Affected,
                Limit::Check,
            )
            .expect("a refused edit is an outcome, not an error of submit");
        let cancelled = f
            .submit(&mut s, &root, &write("a2"), Scope::Affected, Limit::Check)
            .unwrap();
        let c = f
            .submit(&mut s, &root, &write("a3"), Scope::Affected, Limit::Check)
            .unwrap();
        assert!(a < refused && refused < cancelled && cancelled < c);

        assert_eq!(f.cancel(&mut s, cancelled), Ok(true));
        // Known and undelivered, but no longer withdrawable: false, not an error.
        assert_eq!(f.cancel(&mut s, cancelled), Ok(false));
        assert_eq!(f.cancel(&mut s, refused), Ok(false));
        let first = f.next(&mut s, FOREVER).unwrap();
        assert!(
            matches!(&first, Next::Done(id, Err(CheckError::Edit { .. })) if *id == refused),
            "{first:?}"
        );
        assert_eq!(f.next(&mut s, FOREVER), Ok(Next::Withdrawn(cancelled)));
        // The other jobs have finished now: not withdrawn, their outcome is what they produced.
        let last = f.next(&mut s, FOREVER).unwrap();
        assert_eq!(id_of(&last), c, "completion out of submission order");
        assert_eq!(f.cancel(&mut s, a), Ok(false));
        let mut rest = drain(&f, &mut s);
        assert_eq!(rest.len(), 1);
        match rest.remove(0) {
            Next::Done(id, Ok(checked)) => {
                assert_eq!(id, a);
                assert_eq!(
                    f.layer_text(&s, &checked.layer, &rel("src/a.rs")),
                    Ok(Some("a1".to_owned()))
                );
            }
            other => panic!("{other:?}"),
        }
        // The parent is as it was; a delivered id is unknown.
        assert_eq!(
            f.layer_text(&s, &root, &rel("src/a.rs")),
            Ok(Some("a".to_owned()))
        );
        assert_eq!(f.cancel(&mut s, a), Err(CheckError::ForkInUse));

        // An expansion is a job too: one `Expanded` with its id, of the Layer's own state (a file
        // the state created among its files), and withdrawn like any job.
        let created = vec![Edit::write(rel("src/c.rs"), "c")];
        f.submit(&mut s, &root, &created, Scope::Affected, Limit::Check)
            .unwrap();
        let Next::Done(_, Ok(child)) = f.next(&mut s, FOREVER).unwrap() else {
            panic!("the job finishes");
        };
        let targets = [Target::new(rel("src/c.rs"), None)];
        let state = StateFp::of(&created);
        let on_child = Forks::expand(&f, &mut s, &child.layer, state, &targets, 5).unwrap();
        let on_root = Forks::expand(&f, &mut s, &root, StateFp::of(&[]), &targets, 5).unwrap();
        let withdrawn = Forks::expand(&f, &mut s, &root, StateFp::of(&[]), &targets, 5).unwrap();
        assert!(on_child < on_root && on_root < withdrawn);
        assert_eq!(f.cancel(&mut s, withdrawn), Ok(true));
        let mut expanded = BTreeMap::new();
        for next in drain(&f, &mut s) {
            match next {
                Next::Expanded(id, children) => {
                    assert!(expanded.insert(id, children).is_none(), "one outcome");
                }
                Next::Withdrawn(id) => assert_eq!(id, withdrawn),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(expanded.len(), 2);
        drop((root, last, child));
        assert_eq!(f.close(&mut s), Ok(()));
        // C10: what `Expander::expand` gives for the same states with no fork open.
        assert_eq!(
            expanded[&on_child],
            Expander::expand(&f, &mut s, &created, &targets, 5)
        );
        assert_eq!(expanded[&on_child], Ok(Vec::new()));
        assert_eq!(
            expanded[&on_root],
            Expander::expand(&f, &mut s, &[], &targets, 5)
        );
        assert!(matches!(expanded[&on_root], Err(ExpandError::Analysis(_))));
    }

    #[test]
    fn a_done_layer_diagnoses_as_its_flat_composition_for_the_fake() {
        let (f, mut s) = engine_session();
        let root = f.open(&mut s).unwrap();
        let first = vec![Edit::write(rel("src/a.rs"), "an error")];
        let second = vec![
            Edit::write(rel("src/b.rs"), "b2"),
            Edit::write(rel("src/c.rs"), "another error"),
        ];
        f.submit(&mut s, &root, &first, Scope::Affected, Limit::Check)
            .unwrap();
        let Next::Done(_, Ok(parent)) = f.next(&mut s, FOREVER).unwrap() else {
            panic!("the first job finishes");
        };
        let mut job = |scope: Scope| {
            let id = f
                .submit(&mut s, &parent.layer, &second, scope, Limit::Check)
                .unwrap();
            // A sibling running beside it changes nothing.
            f.submit(&mut s, &parent.layer, &first, scope, Limit::Check)
                .unwrap();
            let mut done = None;
            for next in drain(&f, &mut s) {
                if let Next::Done(got, Ok(checked)) = next
                    && got == id
                {
                    done = Some(checked);
                }
            }
            done.expect("the job finishes")
        };
        let workspace = job(Scope::Workspace).findings;
        let affected = job(Scope::Affected);
        let files_of = f.layer_files(&s, &affected.layer, Scope::Affected).unwrap();
        let Checked {
            layer,
            findings: affected,
        } = affected;
        drop((root, parent, layer));
        assert_eq!(f.close(&mut s), Ok(()));

        let base_texts = s.base.clone();
        let base = |p: &RelPath| base_texts.get(p).cloned();
        let composed = crate::domain::search::compose(&first, &second, &base);
        let flat = f.check(&mut s, &composed, Scope::Workspace).unwrap();
        assert_eq!(workspace, flat);
        let affected = &affected;
        assert_eq!(affected.files, vec![rel("src/b.rs"), rel("src/c.rs")]);
        let on_them: Vec<Finding> = flat
            .findings
            .iter()
            .filter(|x| affected.files.contains(&x.path))
            .cloned()
            .collect();
        assert_eq!(affected.findings, on_them);
        assert_eq!(
            files_of,
            vec![rel("src/a.rs"), rel("src/b.rs"), rel("src/c.rs")]
        );
    }

    #[test]
    fn the_fake_refuses_what_forks_refuse() {
        // C6: while forks are open the session's other operations refuse and change nothing.
        let (f, mut s) = engine_session();
        let root = f.open(&mut s).unwrap();
        assert_eq!(
            f.check(&mut s, &[], Scope::Affected),
            Err(CheckError::ForkInUse)
        );
        assert_eq!(f.audit_revert(&mut s), Err(CheckError::ForkInUse));
        assert_eq!(
            Expander::expand(&f, &mut s, &[], &[], 3),
            Err(ExpandError::Session(CheckError::ForkInUse))
        );
        assert_eq!(f.open(&mut s), Err(CheckError::ForkInUse));

        // C5: a Layer of another session, before the poison is looked at.
        let (other_f, mut other) = engine_session();
        other.id = 2;
        let foreign = other_f.open(&mut other).unwrap();
        s.poisoned = true;
        assert_eq!(
            f.submit(&mut s, &foreign, &[], Scope::Affected, Limit::Check),
            Err(CheckError::ForkInUse)
        );
        assert!(s.in_flight.is_empty() && s.ready.is_empty());
        assert!(matches!(
            f.submit(&mut s, &root, &[], Scope::Affected, Limit::Check),
            Err(CheckError::Poisoned(_))
        ));
        drop((root, foreign));
        assert!(matches!(f.close(&mut s), Err(CheckError::Poisoned(_))));
        assert_eq!(other_f.close(&mut other), Ok(()));
    }

    #[test]
    fn a_refused_base_write_poisons_the_fake() {
        let (f, mut s) = engine_session();
        let root = f.open(&mut s).unwrap();
        let edits = [Edit::write(rel("src/a.rs"), "x")];
        let ids: Vec<JobId> = (0..3)
            .map(|_| {
                f.submit(&mut s, &root, &edits, Scope::Affected, Limit::Check)
                    .unwrap()
            })
            .collect();
        s.refused_write = true;
        for id in &ids {
            assert_eq!(f.next(&mut s, FOREVER), Ok(Next::Withdrawn(*id)));
        }
        assert!(matches!(
            f.next(&mut s, FOREVER),
            Err(CheckError::Poisoned(_))
        ));
        drop(root);
        assert!(matches!(f.close(&mut s), Err(CheckError::Poisoned(_))));
    }

    #[test]
    fn a_refused_close_can_be_retried_on_the_fake() {
        let (f, mut s) = engine_session();
        let root = f.open(&mut s).unwrap();
        let edits = [Edit::write(rel("src/a.rs"), "kept")];
        f.submit(&mut s, &root, &edits, Scope::Affected, Limit::Check)
            .unwrap();
        let Next::Done(_, Ok(kept)) = f.next(&mut s, FOREVER).unwrap() else {
            panic!("the job finishes");
        };
        let queued = f
            .submit(&mut s, &root, &edits, Scope::Affected, Limit::Check)
            .unwrap();
        drop(root);

        assert_eq!(
            f.close(&mut s),
            Err(CheckError::ForkInUse),
            "a Layer is held"
        );
        // Closing: the jobs are gone, only reads and `close` are allowed.
        assert_eq!(
            f.submit(&mut s, &kept.layer, &edits, Scope::Affected, Limit::Check),
            Err(CheckError::ForkInUse)
        );
        assert_eq!(f.next(&mut s, FOREVER), Err(CheckError::ForkInUse));
        assert_eq!(f.cancel(&mut s, queued), Err(CheckError::ForkInUse));
        assert_eq!(
            f.check(&mut s, &[], Scope::Affected),
            Err(CheckError::ForkInUse)
        );
        assert_eq!(
            f.layer_text(&s, &kept.layer, &rel("src/a.rs")),
            Ok(Some("kept".to_owned()))
        );
        assert_eq!(
            f.layer_files(&s, &kept.layer, Scope::Affected),
            Ok(vec![rel("src/a.rs")])
        );
        assert_eq!(f.text(&s, &rel("src/a.rs")), Some("a".to_owned()));

        drop(kept);
        assert_eq!(f.close(&mut s), Ok(()));
        assert!(f.check(&mut s, &[], Scope::Affected).is_ok());
        assert!(f.open(&mut s).is_ok());
    }

    #[test]
    fn check_error_reach_is_decided_per_variant() {
        let id = BlueprintId::parse("01890a5d-ac96-774b-bcce-b302099a80aa").unwrap();
        let blueprint_reach = [
            CheckError::Edit {
                path: rel("src/a.rs"),
                reason: "r".to_owned(),
            },
            CheckError::Cancelled,
            CheckError::Panicked("p".to_owned()),
            CheckError::TimedOut { limit_ms: 7 },
            CheckError::DetectorPanicked {
                detector: "d".to_owned(),
                file: rel("src/a.rs"),
            },
        ];
        for e in blueprint_reach {
            assert_eq!(e.reach(), CheckReach::Blueprint, "{e:?}");
            assert!(e.clone().into_blocking(id).is_ok(), "{e:?}");
        }
        let session_reach = [
            CheckError::RevertMismatch {
                paths: vec![rel("src/a.rs")],
            },
            CheckError::Poisoned("x".to_owned()),
            CheckError::ForkInUse,
            CheckError::AlreadyApplied,
            CheckError::Load("l".to_owned()),
        ];
        for e in session_reach {
            assert_eq!(e.reach(), CheckReach::Session, "{e:?}");
            assert_eq!(e.clone().into_blocking(id), Err(e));
        }
        assert_eq!(
            CheckError::TimedOut { limit_ms: 7 }.into_blocking(id),
            Ok(Blocking::BudgetExhausted {
                blueprint: id,
                limit_ms: 7
            })
        );
        assert_eq!(
            CheckError::Edit {
                path: rel("src/a.rs"),
                reason: "r".to_owned()
            }
            .into_blocking(id),
            Ok(Blocking::CheckFailed {
                blueprint: id,
                failure: CheckFailure::Edit {
                    path: rel("src/a.rs"),
                    reason: "r".to_owned()
                }
            })
        );
        assert_eq!(
            CheckError::Cancelled.into_blocking(id),
            Ok(Blocking::CheckFailed {
                blueprint: id,
                failure: CheckFailure::Cancelled
            })
        );
        assert_eq!(
            CheckError::Panicked("p".to_owned()).into_blocking(id),
            Ok(Blocking::CheckFailed {
                blueprint: id,
                failure: CheckFailure::Panicked("p".to_owned())
            })
        );
        assert_eq!(
            CheckError::DetectorPanicked {
                detector: "d".to_owned(),
                file: rel("src/a.rs")
            }
            .into_blocking(id),
            Ok(Blocking::CheckFailed {
                blueprint: id,
                failure: CheckFailure::DetectorPanicked {
                    detector: "d".to_owned(),
                    file: rel("src/a.rs")
                }
            })
        );
    }
}
