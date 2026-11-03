//! Ports the use cases need from the outside world.

use crate::domain::{
    Blocking, Blueprint, BlueprintId, ChangeProbe, CheckFailure, Edit, Finding, JudgeRequest,
    Judgement, Provenance, RelPath, Scope, ScopedFindings, StaticMetrics, Target, UnseenClass,
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
    /// The check was cancelled.
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
            CheckError::Edit { path, reason } => Ok(Blocking::CheckFailed {
                blueprint,
                failure: CheckFailure::Edit { path, reason },
            }),
            CheckError::Cancelled => Ok(Blocking::CheckFailed {
                blueprint,
                failure: CheckFailure::Cancelled,
            }),
            CheckError::Panicked(m) => Ok(Blocking::CheckFailed {
                blueprint,
                failure: CheckFailure::Panicked(m),
            }),
            CheckError::DetectorPanicked { detector, file } => Ok(Blocking::CheckFailed {
                blueprint,
                failure: CheckFailure::DetectorPanicked { detector, file },
            }),
            session @ (CheckError::RevertMismatch { .. }
            | CheckError::Poisoned(_)
            | CheckError::ForkInUse
            | CheckError::AlreadyApplied
            | CheckError::Load(_)) => Err(session),
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
    ///   tested: ryo-v2-ra tests/stack.rs a_finding_is_on_a_file_of_its_diagnosis
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

/// A [`Verifier`] whose forks take stacked writes: a child state is written on top of its
/// parent's, never rebuilt from the base.
///
/// A fork is a stack of levels over the session's base state. `write` pushes a level whose edits
/// are written on top of the level below and resolved against that level's state, as a load of
/// that state would resolve them. `fork_text` and `diagnose` read the top level. `pop` reverts the
/// top level. `release` reverts every level, top first, and ends the fork.
///
/// A level is diagnosed and reverted under the rules of a check:
/// - the check time limit cuts a diagnosis off;
/// - a detector panic is judged against the baseline;
/// - a revert is compared with the level below, and a revert that does not restore it, or that
///   cannot be carried out or confirmed, poisons the session.
///
/// The verdict of a level is against the baseline, like a check's. `diagnose` covers the scope of
/// the top level's own edits, and a file outside it has the findings it has at the level below.
/// So a caller that judges the state of a level keeps the findings of the levels below.
///
/// The reads of [`Verifier`] describe the base state, never a level: while a fork is live, and
/// after a release that found the session poisoned.
///
/// An adapter whose forks write into the session's one database allows one live fork per
/// session, and while it is live the session's other operations refuse with `ForkInUse`, except an
/// expansion of its top level. An adapter whose forks are separate copies may keep several live.
///
/// # Contract
/// - C1: on a poisoned session `fork`, and given the live fork's ticket `write`, `diagnose`,
///   `files_of`, `fork_text` and `pop`, are `Poisoned`; `levels` still answers; `release` ends
///   the fork and is `Poisoned`. A ticket that is not the live fork's is `ForkInUse` before the
///   poison is looked at, and changes nothing.
///   tested: ryo-v2-ra tests/stack.rs poisoned_session_refuses_stacked_operations
///   tested: ryo-v2-ra tests/stack.rs a_ticket_of_another_session_is_refused
/// - C2: while a fork of an adapter with one live fork per session is live, `Verifier::check`,
///   `RevertAudit::audit_revert` and a second `fork` are `ForkInUse`, or `Poisoned` on a poisoned
///   session, before they change anything, and so is `Expander::expand` (as
///   `ExpandError::Session`) while that fork is an `Analyzer` fork; an operation given a ticket
///   that is not the live fork's is `ForkInUse`.
///   tested: ryo-v2-ra tests/stack.rs session_operations_refuse_while_a_fork_is_live
///   tested: ryo-v2-ra tests/stack.rs a_ticket_of_another_session_is_refused
/// - C3: while a fork is live, and after a release that found the session poisoned,
///   `Verifier::text`, `Verifier::provenance` and `Verifier::baseline` describe the base state,
///   never a level.
///   tested: ryo-v2-ra tests/stack.rs verifier_reads_describe_the_base_while_a_fork_is_live
///   tested: ryo-v2-ra tests/stack.rs poisoned_session_refuses_stacked_operations
/// - C4: `Verifier::check` of edits at a scope gives what `fork`, one `write` of those edits,
///   `diagnose` at that scope and `release` give; when more than one fails, the failure of
///   `release` wins, then that of `write`, then that of `diagnose`.
///   tested: ryo-v2-ra tests/stack.rs check_is_the_one_level_case
///   tested: ryo-v2-ra tests/stack.rs a_session_error_of_the_release_wins_in_a_check
pub trait Stacked: Verifier {
    /// The ticket of a live fork of the session's base state; its levels live behind the
    /// session.
    type Fork;

    /// Opens a fork of the session's base state, with no level.
    ///
    /// A poisoned session is `Poisoned`; a session on which a fork is live is `ForkInUse`.
    fn fork(&self, s: &mut Self::Session) -> Result<Self::Fork, CheckError>;

    /// Pushes a level that writes `edits` on top of the top level.
    ///
    /// A write that panics the database poisons the session and is `Poisoned`, not `Panicked`, so
    /// that a failure of reach `Blueprint` always leaves the levels as they were.
    ///
    /// # Contract
    /// - C1: pushes one level on top of the top level: `levels` grows by one, and `fork_text` and
    ///   `diagnose` read the new level.
    ///   tested: ryo-v2-ra tests/stack.rs levels_stack_and_pop_back_to_the_level_below
    /// - C2: edits resolve against the state of the top level: a path that is not a file of that
    ///   state is created, one a lower level deleted included; a path that is a file of it is
    ///   modified or deleted; deleting a path that is not a file of it is `Edit`.
    ///   tested: ryo-v2-ra tests/stack.rs a_level_writes_against_the_state_below
    /// - C3: a stack diagnoses as its flat composition: at level k, `diagnose` at
    ///   `Scope::Workspace` gives what `Verifier::check` at `Scope::Workspace` gives for the edits
    ///   that take the base to the state of level k (the same files, findings and `unseen_at`, or
    ///   the same failure of its diagnosis), and at `Scope::Affected` its files are the top
    ///   level's scope and their findings are those that check gives on them.
    ///   tested: ryo-v2-ra tests/stack.rs a_stack_diagnoses_as_its_flat_composition
    /// - C4: a refused write (`Edit`, or a failure of the diagnosis it takes of the level below)
    ///   pushes no level and leaves every text as it was.
    ///   tested: ryo-v2-ra tests/stack.rs a_refused_write_pushes_no_level
    /// - C5: a write with no edit pushes a level whose `Scope::Affected` diagnosis covers no file
    ///   and names no unseen class.
    ///   tested: ryo-v2-ra tests/stack.rs an_empty_write_diagnoses_nothing
    /// - C6: a write the database cannot carry out (its write panics) is `Poisoned`, pushes no
    ///   level and poisons the session; the reads of `Verifier` still describe the base.
    ///   tested: ryo-v2-ra tests/stack.rs a_database_write_that_panics_poisons_the_session
    fn write(
        &self,
        s: &mut Self::Session,
        f: &mut Self::Fork,
        edits: &[Edit],
    ) -> Result<(), CheckError>;

    /// The number of levels of the live fork.
    ///
    /// # Contract
    /// - C1: the number of levels of the live fork, on a poisoned session too; a ticket that is
    ///   not the live fork's is `ForkInUse`.
    ///   tested: ryo-v2-ra tests/stack.rs levels_stack_and_pop_back_to_the_level_below
    ///   tested: ryo-v2-ra tests/stack.rs pop_compares_texts_and_poisons_on_a_difference
    ///   tested: ryo-v2-ra tests/stack.rs a_ticket_of_another_session_is_refused
    fn levels(&self, s: &Self::Session, f: &Self::Fork) -> Result<usize, CheckError>;

    /// The text of `path` at the top level.
    ///
    /// # Contract
    /// - C1: the text of `path` at the top level (the base text with no level); `Ok(None)` when
    ///   `path` is not a file of that state (not indexed, deleted by a level, never created); an
    ///   error when the text could not be read.
    ///   tested: ryo-v2-ra tests/stack.rs levels_stack_and_pop_back_to_the_level_below
    ///   tested: ryo-v2-ra tests/stack.rs a_level_writes_against_the_state_below
    fn fork_text(
        &self,
        s: &Self::Session,
        f: &Self::Fork,
        path: &RelPath,
    ) -> Result<Option<String>, CheckError>;

    /// Diagnoses the top level at `scope`.
    ///
    /// # Contract
    /// - C1: `Scope::Affected` covers the files of the crates the top level's edits touch and of
    ///   their dependents, at the top level's state (files it created included, files it deleted
    ///   excluded); `Scope::Workspace` covers every file of that state.
    ///   tested: ryo-v2-ra tests/stack.rs affected_scope_is_the_top_levels
    /// - C2: with no level the result is `ScopedFindings::default()`.
    ///   tested: ryo-v2-ra tests/stack.rs pop_without_a_level_is_false
    /// - C3: a file outside the top level's `Scope::Affected` has, at the top level, the findings
    ///   it has at the level below, and the same files under each class of `unseen_at`.
    ///   tested: ryo-v2-ra tests/stack.rs files_outside_the_top_scope_keep_the_level_below
    /// - C4: the adapter's check time limit cuts a diagnosis off with `TimedOut`; the levels stay
    ///   and the fork stays usable.
    ///   tested: ryo-v2-ra tests/stack.rs a_cut_off_diagnosis_keeps_the_levels
    /// - C5: the unseen classes of the result are those this diagnosis observed at the top level;
    ///   a class observed at a level below and not at the top is not among them.
    ///   tested: ryo-v2-ra tests/stack.rs a_level_names_the_unseen_it_observes
    /// - C6: a detector that panics on a file of the diagnosis, where it did not panic on that
    ///   file at the baseline, makes the result `DetectorPanicked` (the first by detector and
    ///   file), whatever the levels below had; one that panicked on that file at the baseline too
    ///   names its class among the unseen classes, whatever the levels below had. Either way the
    ///   levels stay and the fork stays usable.
    ///   tested: ryo-v2-ra tests/stack.rs a_new_detector_panic_fails_the_diagnose_and_keeps_the_level
    ///   tested: ryo-v2-ra tests/stack.rs a_baseline_panic_brought_back_above_is_named
    ///   tested: ryo-v2-ra tests/stack.rs a_baseline_detector_panic_is_named_at_every_level
    /// - C7: the `check_ms` of the `Event::Checked` of a diagnosis is the duration of the write of
    ///   the top level plus the duration of this diagnosis: an earlier diagnosis of the level,
    ///   the time between calls and the pop that made the level the top are not in it.
    ///   tested: ryo-v2-ra tests/stack.rs the_diagnoses_of_a_level_do_not_add_up
    /// - C8: every class of the result's `unseen_at` names at least one file, and only files of
    ///   the result; by `Stacked` C4 so does every class of a check's findings.
    ///   tested: ryo-v2-ra tests/stack.rs a_diagnosis_names_where_it_observed_each_unseen_class
    /// - C9: every finding of the result is on one of the result's `files`, one the analyzer
    ///   attributes to another file than the one it scanned included; by `Stacked` C4 so is every
    ///   finding of a check. A finding below is so on a file of its own diagnosis, and a file
    ///   diagnosed again replaces it (`ScopedFindings::on_top_of`).
    ///   tested: ryo-v2-ra tests/stack.rs a_finding_is_on_a_file_of_its_diagnosis
    fn diagnose(
        &self,
        s: &Self::Session,
        f: &Self::Fork,
        scope: Scope,
    ) -> Result<ScopedFindings, CheckError>;

    /// The files a one-level `Verifier::check` at `scope` covers for the edits that take the base
    /// to the top level's state. The adapter derives those edits' paths from its own levels, so
    /// a caller cannot name a wrong set. The caller keeps a composed verdict to these files
    /// (`Verdict::of_level`).
    ///
    /// # Contract
    /// - C1: the files, sorted, are those of `Verifier::check` at `scope` of the edits that take
    ///   the base to the top level's state: one edit per path some level edited whose presence or
    ///   text at the top level differs from the base, and no other.
    ///   tested: ryo-v2-ra tests/stack.rs files_of_and_of_level_equal_the_check_over_a_stack
    /// - C2: with no level, or with every path the levels edited back at its base presence and
    ///   text, the result at `Scope::Affected` is empty, as a check with no edit diagnoses no file
    ///   there; at `Scope::Workspace` it is every file of the top level's state, as such a check
    ///   diagnoses every file.
    ///   tested: ryo-v2-ra tests/stack.rs files_of_and_of_level_equal_the_check_over_a_stack
    fn files_of(
        &self,
        s: &Self::Session,
        f: &Self::Fork,
        scope: Scope,
    ) -> Result<Vec<RelPath>, CheckError>;

    /// Reverts the top level.
    ///
    /// Given the live fork's ticket, every error of a pop poisons the session (`RevertMismatch`,
    /// or `Poisoned`, which is also the answer when the session was poisoned already), so a pop
    /// never fails with a `CheckError` of reach `Blueprint`. A ticket that is not the live fork's
    /// is `ForkInUse`.
    ///
    /// # Contract
    /// - C1: reverts the top level and returns `true`: afterwards every text is that of the level
    ///   below and `levels` is one less.
    ///   tested: ryo-v2-ra tests/stack.rs levels_stack_and_pop_back_to_the_level_below
    /// - C2: a revert that does not restore the level below (its texts, or the adapter's
    ///   diagnostic re-assert) is `RevertMismatch`, removes the level and poisons the session; a
    ///   revert that cannot be carried out or confirmed (a database write panics, or the
    ///   re-assert cannot run) is `Poisoned`, removes the level and poisons the session.
    ///   tested: ryo-v2-ra tests/stack.rs pop_compares_texts_and_poisons_on_a_difference
    ///   tested: ryo-v2-ra tests/stack.rs pop_reassert_compares_with_the_level_below
    ///   tested: ryo-v2-ra tests/stack.rs a_revert_that_cannot_be_confirmed_poisons_the_session
    ///   tested: ryo-v2-ra tests/stack.rs a_database_write_that_panics_poisons_the_session
    /// - C3: with no level `pop` changes nothing and returns `Ok(false)`.
    ///   tested: ryo-v2-ra tests/stack.rs pop_without_a_level_is_false
    fn pop(&self, s: &mut Self::Session, f: &mut Self::Fork) -> Result<bool, CheckError>;

    /// Reverts every level and ends the fork.
    ///
    /// # Contract
    /// - C1: reverts every level, top first, under the rules of `pop`, and ends the fork: a later
    ///   `fork` is not `ForkInUse`.
    ///   tested: ryo-v2-ra tests/stack.rs release_reverts_every_level
    /// - C2: every revert failure poisons the session, so from the first failure on, or on a
    ///   session already poisoned, it writes nothing more: it drops the remaining levels, keeping
    ///   their base texts for the reads of `Verifier`, ends the fork and returns that failure
    ///   (`Poisoned` for a session already poisoned).
    ///   tested: ryo-v2-ra tests/stack.rs release_stops_writing_after_a_failure
    ///   tested: ryo-v2-ra tests/stack.rs poisoned_session_refuses_stacked_operations
    fn release(&self, s: &mut Self::Session, f: Self::Fork) -> Result<(), CheckError>;
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
    /// the state's text.
    ///
    /// When a stacked fork is live on the session, `state` is the state of its top level, and the
    /// children are read there: nothing is written.
    ///
    /// # Contract
    /// - C1: with a stacked fork live on the session and `state` the state of its top level, the
    ///   result is what `expand` gives for `state` with no fork live, and afterwards the fork has
    ///   the same levels and every text is as before.
    ///   tested: ryo-v2-ra tests/stack.rs expand_reads_the_top_level_of_a_live_fork
    fn expand(
        &self,
        s: &mut Self::Session,
        state: &[Edit],
        targets: &[Target],
        max: usize,
    ) -> Result<Vec<Blueprint>, ExpandError>;
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

    fn rel(s: &str) -> RelPath {
        RelPath::parse(s).unwrap()
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
