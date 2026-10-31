//! Ports the use cases need from the outside world.

use crate::domain::{Edit, Finding, RelPath, Scope, ScopedFindings};
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
    /// Diagnostics after revert differ from the baseline.
    #[error("revert did not restore the baseline")]
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
}

fn join_paths(paths: &[RelPath]) -> String {
    paths
        .iter()
        .map(RelPath::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// What `apply_blueprints` needs from the analyzer: the texts it holds, and a check of edits
/// that leaves no trace.
pub trait Verifier {
    /// A loaded workspace.
    type Session;
    /// The analyzer's current text of `path`; `None` if it is not a workspace file.
    fn text(&self, s: &Self::Session, path: &RelPath) -> Option<String>;
    /// What the workspace model says about `path`.
    fn facts(&self, s: &Self::Session, path: &RelPath) -> FileFacts;
    /// Every Error at load (all local files), message included.
    fn baseline(&self, s: &Self::Session) -> Vec<Finding>;
    /// Applies `edits`, diagnoses the files of `scope`, and reverts. The observable state of
    /// the session afterwards is the state before.
    fn check(
        &self,
        s: &mut Self::Session,
        edits: &[Edit],
        scope: Scope,
    ) -> Result<ScopedFindings, CheckError>;
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
    /// (the cli reads `/proc/self/status`; the core never reads it).
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
    /// 適用を巻き戻して Fork を捨てる。assert_revert 有効時、巻き戻し後の診断が baseline と違えば Err(RevertMismatch)。
    fn discard(&self, session: &mut Self::Session, fork: Self::Fork) -> Result<(), AnalyzerError>;
}
