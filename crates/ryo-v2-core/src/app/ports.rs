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
    /// A fork is live on the session: the operation needs the session at its base state.
    #[error("a fork was already live on the session")]
    ForkInUse,
    /// The adapter applied edits twice to one fork.
    #[error("edits were applied twice to one fork")]
    AlreadyApplied,
    /// The analyzer reported a load failure during a check.
    #[error("load failed: {0}")]
    Load(String),
    /// A detector panicked on a file the check read, so the check cannot judge that file. The
    /// session was restored.
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
/// every check, whatever its result, and a difference is `RevertMismatch`. So an `Edit`,
/// `Cancelled`, `Panicked` or `DetectorPanicked` that comes back says the session was restored
/// and only this Blueprint's check did not finish.
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
    /// `RevertMismatch` and `Poisoned` are the poison of the session; `ForkInUse` and
    /// `AlreadyApplied` mean the adapter's fork bookkeeping is broken; `Load` means the session
    /// itself failed. None of these can be pinned on the Blueprint.
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
    /// the state's text.
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
    /// A scoped check finished.
    Checked {
        /// Files diagnosed.
        scope_files: usize,
        /// Local files at load.
        total_files: usize,
        /// Check time.
        check_ms: u64,
    },
    /// A fork was discarded.
    Reverted {
        /// Revert time.
        revert_ms: u64,
        /// Whether the post-revert diagnostics matched baseline (None = not asserted).
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
