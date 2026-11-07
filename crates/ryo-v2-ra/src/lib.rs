//! rust-analyzer adapter of ryo-v2.
//!
//! The only crate that depends on the `ra_ap_*` stack. It implements the ports of
//! `ryo-v2-core::app::ports` named by its `impl` blocks: load a workspace once, fork its in-memory
//! state per candidate, write text edits (stacked on earlier writes through `Stacked`), collect
//! HIR diagnostics, revert. Nothing here runs cargo or rustc per candidate.

mod assess;
mod check;
mod diagnostics;
mod duplicate;
mod engine;
mod expand;
mod field_bounds;
mod flat;
mod fork;
mod forks;
mod memory;
mod rev_deps;
mod session;
mod stack;
mod unresolved;

use ryo_v2_core::app::ports::{Analyzer, AnalyzerError, Clock, Observer};
use ryo_v2_core::domain::{Edit, Finding, RelPath, Scope, ScopedFindings};
use std::path::Path;
use std::sync::Arc;

pub use assess::SyntaxAssessor;
pub use diagnostics::{FileTimes, GapDetector, ThreadCount};
pub use fork::RaFork;
pub use forks::RaLayer;
pub use memory::{IngredientRow, MemoRow, MemoryReport};
/// A layer's own counters on the fork Engine ([`RaSession::level_stats`]).
pub use ryo_v2_engine::LayerStats;
pub use session::{DetectorPanic, ExpanderPanic, RaSession};
pub use stack::RaStack;

/// Configuration of [`RaAnalyzer`].
#[derive(Debug, Clone)]
pub struct RaConfig {
    /// sysroot の proc-macro srv で derive 等を展開する。
    pub proc_macro: bool,
    /// 診断の並列度。None = available_parallelism。
    pub threads: Option<usize>,
    /// Every write is sampled: the end of each level (a discard, a pop, a check's) and of each
    /// `Forks` job checks the isolation of the base (I2), and every flat reference check
    /// re-diagnoses the scope of its edits after its write-back and compares it with the
    /// baseline.
    pub assert_revert: bool,
    /// ra が診断しない error の detector を走らせる: 未解決の型名と path 先頭 (E0405/E0425/E0433)、
    /// item と `use` が同じ module で同じ名前を定義する (E0255)、derive した impl と手書きの
    /// `Copy` impl が field に求める trait (E0204/E0277/E0369)。全ての [`GapDetector`] を一斉に切り替える。以前の名前は `unresolved_paths`(最初の detector の名前)。
    pub gap_detectors: bool,
    /// Every n-th write (a `Verifier::check` or a `Stacked::write`) is sampled: the end of its
    /// level checks the isolation of the base (I2). None = never.
    ///
    /// # Contract
    /// - C1: `Some(n)` samples every n-th write, a check being one write (the 1st of `Some(2)`
    ///   is not, the 2nd is); `None` never does through this field.
    ///   tested: tests/verifier.rs assert_revert_every_samples_the_diagnostic_assert
    ///   tested: tests/stack.rs writes_count_for_the_sampled_isolation
    pub assert_revert_every: Option<std::num::NonZeroU32>,
    /// `CargoConfig::set_test`: `#[cfg(test)]` code of workspace members is enabled, so it is diagnosed.
    ///
    /// # Contract
    /// - C1: with the default `true` an error inside a `#[cfg(test)]` module is in the baseline;
    ///   with `false` it is not.
    ///   tested: tests/verifier.rs cfg_test_config_diagnoses_test_module
    ///   tested: tests/verifier.rs ra_config_default_diagnoses_test_code
    pub cfg_test: bool,
    /// The forks that run side by side on the fork Engine (`Forks`, D11 stage 3b); read at
    /// `Forks::open`. Every fork of the adapter (`Forks`, `Stacked`, `Analyzer`, and a check,
    /// which is a one-level fork) is layers over the frozen base, which is never written; the
    /// stacked undo log was deleted at stage 4, and the flat reference check
    /// (`RaAnalyzer::reference_check`) is the one write of the base left.
    pub forks: ForksConfig,
    /// The proc-macro server processes the load starts (with `proc_macro`), fixed for the
    /// session: the jobs of `Forks` share them.
    ///
    /// # Contract
    /// - C1: `load` refuses a process whose environment sets `RUST_ANALYZER_USE_POSTCARD`
    ///   (`AnalyzerError::Load`): with it the proc-macro server's bidirectional protocol runs
    ///   salsa queries of a layer while it holds a server's lock, a lock cycle with the claims of
    ///   the layer's other threads that salsa cannot see once forks run side by side (D11, 3b).
    ///   tested: session::tests::postcard_protocol_is_refused_at_load
    pub proc_macro_processes: std::num::NonZeroUsize,
}

/// How [`RaAnalyzer`]'s `Forks` shares the session's threads (`RaConfig.threads`, T) among the
/// jobs it runs at once.
///
/// With N = `width` and c = 1 when `caller_works` (else 0), the workers' share is
/// `T_w = max(1, T - c)`, `Forks::width` is `W = min(N, T_w)` and a job's diagnosis runs on
/// `t = max(1, T_w / W)` threads, so the busy threads, the caller's included, are at most T.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForksConfig {
    /// N, the jobs asked to run at once; `None` = the workers' whole share, `T_w`.
    pub width: Option<std::num::NonZeroUsize>,
    /// Whether the caller's own thread works between its calls (it expands, judges, assesses):
    /// it is then counted out of the workers' share. `false` for a caller that only waits in
    /// `Forks::next` (a bench).
    pub caller_works: bool,
}

impl Default for ForksConfig {
    fn default() -> Self {
        Self {
            width: None,
            caller_works: true,
        }
    }
}

/// The proc-macro server processes a load starts by default (`RaConfig.proc_macro_processes`).
const PROC_MACRO_PROCESSES: std::num::NonZeroUsize = match std::num::NonZeroUsize::new(2) {
    Some(n) => n,
    None => std::num::NonZeroUsize::MIN,
};

impl Default for RaConfig {
    fn default() -> Self {
        Self {
            proc_macro: true,
            threads: None,
            assert_revert: false,
            gap_detectors: true,
            assert_revert_every: None,
            cfg_test: true,
            forks: ForksConfig::default(),
            proc_macro_processes: PROC_MACRO_PROCESSES,
        }
    }
}

/// The rust-analyzer implementation of the `Analyzer` port.
pub struct RaAnalyzer {
    pub(crate) config: RaConfig,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) observer: Arc<dyn Observer>,
    /// Cut-off of the diagnostics of one diagnosis of a level (a check's included), in ms. A field
    /// of the analyzer and not of `RaConfig`, so literals of `RaConfig` elsewhere keep compiling.
    pub(crate) check_timeout_ms: Option<u64>,
    /// Detector and file on which it is made to panic or to raise a salsa cancellation; a test
    /// hook.
    pub(crate) detector_fault: Option<DetectorFault>,
    /// File whose text a revert gets wrong; a test hook.
    pub(crate) revert_fault: Option<RelPath>,
    /// Whether the diagnostic re-assert of a revert fails to run; a test hook.
    pub(crate) reassert_fault: bool,
    /// Which database write of the texts panics; a test hook.
    pub(crate) db_write_panic: Option<DbWritePanic>,
    /// File whose base text cannot be read; a test hook.
    pub(crate) db_read_fault: Option<RelPath>,
    /// Whether the diagnosis of every revert audit fails to run; a test hook.
    pub(crate) audit_fault: bool,
    /// Whether every diagnosis of a level panics before it diagnoses a file; a test hook.
    pub(crate) diagnose_panic: bool,
    /// Counts the threads diagnosing at once; a test hook.
    pub(crate) thread_count: Option<Arc<ThreadCount>>,
    /// Records each file's diagnosis time; a measurement hook.
    pub(crate) file_times: Option<Arc<FileTimes>>,
    /// Runs on a `Forks` job's worker between its writes and its diagnosis; a test hook.
    pub(crate) job_hook: Option<JobHook>,
    /// Whether every `Forks` job writes the frozen base after its own writes; a test hook.
    pub(crate) job_base_write: bool,
}

/// A test hook run on the worker of a `Forks` job ([`RaAnalyzer::with_job_hook`]).
pub(crate) type JobHook = Arc<dyn Fn(ryo_v2_core::app::ports::JobId) + Send + Sync>;

/// The fault the test hooks `RaAnalyzer::with_detector_fault` and
/// `RaAnalyzer::with_detector_cancel` put on one detector and file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DetectorFault {
    pub(crate) detector: GapDetector,
    pub(crate) path: RelPath,
    /// A salsa cancellation instead of a panic.
    pub(crate) cancel: bool,
}

/// The database write of the texts that the test hook `RaAnalyzer::with_db_write_panic` makes
/// panic.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DbWritePanic {
    /// The write of a level's edits, or of the flat reference check's.
    Write,
    /// The flat reference check's write-back of the base texts.
    Revert,
}

impl RaAnalyzer {
    /// Builds an analyzer; `clock` times the events that go to `observer`. No check is cut off.
    pub fn new(config: RaConfig, clock: Arc<dyn Clock>, observer: Arc<dyn Observer>) -> Self {
        Self {
            config,
            clock,
            observer,
            check_timeout_ms: None,
            detector_fault: None,
            revert_fault: None,
            reassert_fault: false,
            db_write_panic: None,
            db_read_fault: None,
            audit_fault: false,
            diagnose_panic: false,
            thread_count: None,
            file_times: None,
            job_hook: None,
            job_base_write: false,
        }
    }

    /// Counts, in `count`, the threads that diagnose files at once and the most one diagnosis
    /// spreads over, in every diagnosis this analyzer runs. For the tests of the thread budget of
    /// `Forks`; nothing else sets it.
    #[doc(hidden)]
    pub fn with_thread_count(mut self, count: Option<Arc<ThreadCount>>) -> RaAnalyzer {
        self.thread_count = count;
        self
    }

    /// Records, in `times`, the wall time of each file's diagnosis in every diagnosis this
    /// analyzer runs (`diagnostics::FileTimes`). For `bench --file-times`, which locates where a
    /// check's time goes (stage 4 gate RS); it changes no finding.
    #[doc(hidden)]
    pub fn with_file_times(mut self, times: Option<Arc<FileTimes>>) -> RaAnalyzer {
        self.file_times = times;
        self
    }

    /// Runs `hook` with the job's id on the worker of every `Forks` job, after the job's writes
    /// and before its diagnosis (a delay, a rendezvous, a raised salsa cancellation). For the
    /// tests of jobs side by side; nothing else sets it.
    #[doc(hidden)]
    pub fn with_job_hook(
        mut self,
        hook: Option<Arc<dyn Fn(ryo_v2_core::app::ports::JobId) + Send + Sync>>,
    ) -> RaAnalyzer {
        self.job_hook = hook;
        self
    }

    /// Makes every `Forks` job, after its writes, create an input of the frozen base (a
    /// singleton, whose index the base and every layer share): a write the base refuses and
    /// counts. For the tests of the isolation guard (I2); nothing else sets it.
    #[doc(hidden)]
    pub fn with_job_base_write(mut self, write: bool) -> RaAnalyzer {
        self.job_base_write = write;
        self
    }

    /// Makes every diagnosis of a level (a check's included) panic inside the level, before it
    /// diagnoses a file, as a panic of rust-analyzer in a query of that level would. For the
    /// tests of the poison of a layer; nothing else sets it.
    #[doc(hidden)]
    pub fn with_diagnose_panic(mut self, panic: bool) -> RaAnalyzer {
        self.diagnose_panic = panic;
        self
    }

    /// Makes that detector panic on that file in every collect this analyzer runs.
    /// For the tests of the containment and of the re-assert; nothing else sets it.
    #[doc(hidden)]
    pub fn with_detector_fault(mut self, fault: Option<(GapDetector, RelPath)>) -> RaAnalyzer {
        self.detector_fault = fault.map(|(detector, path)| DetectorFault {
            detector,
            path,
            cancel: false,
        });
        self
    }

    /// Makes that detector raise a salsa cancellation on that file in every collect this
    /// analyzer runs, as a query of it does when its database is cancelled. For the tests of the
    /// mapping of a cancellation (it is `Cancelled`, never a panic); nothing else sets it.
    #[doc(hidden)]
    pub fn with_detector_cancel(mut self, at: Option<(GapDetector, RelPath)>) -> RaAnalyzer {
        self.detector_fault = at.map(|(detector, path)| DetectorFault {
            detector,
            path,
            cancel: true,
        });
        self
    }

    /// Makes every write-back of the flat reference check that restores `path` (a file it
    /// modified or deleted) restore it with its stored text plus one byte, so that the comparison
    /// of the texts must catch it. For the tests of that comparison; nothing else sets it.
    #[doc(hidden)]
    pub fn with_revert_fault(mut self, path: Option<RelPath>) -> RaAnalyzer {
        self.revert_fault = path;
        self
    }

    /// Makes the check after a write fail to run: the re-assert of every flat reference check
    /// (with `assert_revert`), after the texts are written back, and the isolation check of the
    /// base at every sampled pop. For the tests of a write that cannot be confirmed; nothing
    /// else sets it.
    #[doc(hidden)]
    pub fn with_reassert_fault(mut self, fault: bool) -> RaAnalyzer {
        self.reassert_fault = fault;
        self
    }

    /// Makes that database write of the texts panic, before it changes the database: a level's
    /// write (`Write`; the flat reference check's too), or the flat reference check's write-back
    /// (`Revert`). For the tests of a write the database cannot carry out; nothing else sets it.
    #[doc(hidden)]
    pub fn with_db_write_panic(mut self, at: Option<DbWritePanic>) -> RaAnalyzer {
        self.db_write_panic = at;
        self
    }

    /// Makes every base read of `path` fail as a database read that panics would. For the tests
    /// of the paths that report such a failure; nothing in the adapter sets it.
    #[doc(hidden)]
    pub fn with_db_read_fault(mut self, path: Option<RelPath>) -> RaAnalyzer {
        self.db_read_fault = path;
        self
    }

    /// Makes the diagnosis of every revert audit fail as a diagnostic thread that panics would,
    /// before it diagnoses a file. For the tests of an audit that cannot run; nothing else sets
    /// it.
    #[doc(hidden)]
    pub fn with_audit_fault(mut self, fault: bool) -> RaAnalyzer {
        self.audit_fault = fault;
        self
    }

    /// Cut-off of the diagnostics of one diagnosis of a level (a check's included); `None` (the
    /// default of `new`) = no cut-off. Baseline, revert assert, revert audit and expansion are
    /// never cut off.
    ///
    /// # Contract
    /// - C1: with `Some(limit_ms)` a diagnosis of a level, a check's included, that reaches the
    ///   limit ends in `CheckError::TimedOut`; with `None` it never does.
    ///   tested: tests/timeout.rs check_past_timeout_is_timed_out_and_session_usable
    ///   tested: tests/stack.rs a_cut_off_diagnosis_keeps_the_levels
    pub fn with_check_timeout(mut self, limit_ms: Option<u64>) -> RaAnalyzer {
        self.check_timeout_ms = limit_ms;
        self
    }
}

/// The adapter's `Analyzer`.
///
/// # Contract
/// - C1: a `discard` re-asserts no revert: it drops the fork's layer and, sampled, checks the
///   isolation of the base, which a detector fault does not touch; the session stays usable.
///   tested: tests/verifier.rs a_sampled_discard_reasserts_no_revert
/// - C2: a `discard` writes nothing back: a fault of the write-back hooks fails none, and every
///   text is the base's after it.
///   tested: tests/stack.rs analyzer_discard_writes_nothing_back
/// - C3: on a session poisoned while its fork is live, `apply` and `diagnostics` are refused with
///   `AnalyzerError::Poisoned` and write nothing; `discard` ends the fork without writing.
///   tested: tests/stack.rs a_poisoned_fork_is_not_written
/// - C4: `apply`, `diagnostics` and `discard` with a fork that is not the session's live one
///   (another session's, or one already discarded) are `ForkInUse` and change nothing.
///   tested: tests/stack.rs an_analyzer_ticket_of_another_session_is_refused
/// - C5: while a fork is live, `Verifier::check`, `Expander::expand` and
///   `RevertAudit::audit_revert` are `ForkInUse`, and `Verifier::text` describes the base.
///   tested: tests/stack.rs a_live_analyzer_fork_holds_the_session
/// - C6: the files and findings of `diagnostics` after `apply` are those of `Stacked::diagnose`
///   after one `Stacked::write` of the same edits, and the texts after the discard are those after
///   a release, where the diagnosis meets neither the time limit nor a new detector panic.
///   tested: tests/stack.rs one_level_matches_the_analyzer_path
/// - C7: on a poisoned session `fork`, `apply`, `diagnostics` and `discard` are
///   `AnalyzerError::Poisoned`, never `Panicked`, so that `app::verify::verify` gives the poison
///   as it is and `Expander::expand` gives `ExpandError::Session` with `CheckError::Poisoned`,
///   which reaches the session, not one Blueprint.
///   tested: tests/stack.rs a_poisoned_session_is_poisoned_on_the_analyzer_port
impl Analyzer for RaAnalyzer {
    type Session = RaSession;
    type Fork = RaFork;

    fn load(&self, root: &Path) -> Result<RaSession, AnalyzerError> {
        session::load(self, root)
    }

    fn baseline(&self, session: &RaSession) -> Vec<Finding> {
        session.baseline.clone()
    }

    fn fork(&self, session: &mut RaSession) -> Result<RaFork, AnalyzerError> {
        fork::fork(session)
    }

    fn apply(
        &self,
        session: &mut RaSession,
        fork: &mut RaFork,
        edits: &[Edit],
    ) -> Result<(), AnalyzerError> {
        fork::apply(self, session, fork, edits)
    }

    fn diagnostics(
        &self,
        session: &RaSession,
        fork: &RaFork,
        scope: Scope,
    ) -> Result<ScopedFindings, AnalyzerError> {
        fork::diagnostics(self, session, fork, scope)
    }

    fn discard(&self, session: &mut RaSession, fork: RaFork) -> Result<(), AnalyzerError> {
        fork::discard(self, session, &fork)
    }
}
