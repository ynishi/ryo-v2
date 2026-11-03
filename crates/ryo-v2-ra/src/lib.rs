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
mod fork;
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
pub use diagnostics::GapDetector;
pub use fork::RaFork;
pub use memory::{IngredientRow, MemoRow, MemoryReport};
pub use session::{DetectorPanic, ExpanderPanic, RaSession};
pub use stack::RaStack;

/// Configuration of [`RaAnalyzer`].
#[derive(Debug, Clone)]
pub struct RaConfig {
    /// sysroot の proc-macro srv で derive 等を展開する。
    pub proc_macro: bool,
    /// 診断の並列度。None = available_parallelism。
    pub threads: Option<usize>,
    /// Every revert of a level (a discard, a pop, a check's) re-diagnoses the scope of its edits
    /// and compares it with the level below.
    pub assert_revert: bool,
    /// ra が診断しない error の detector を走らせる: 未解決の型名と path 先頭 (E0405/E0425/E0433)、
    /// item と `use` が同じ module で同じ名前を定義する (E0255)、derive した impl と手書きの
    /// `Copy` impl が field に求める trait (E0204/E0277/E0369)。全ての [`GapDetector`] を一斉に切り替える。以前の名前は `unresolved_paths`(最初の detector の名前)。
    pub gap_detectors: bool,
    /// Every n-th write (a `Verifier::check` or a `Stacked::write`) re-diagnoses the scope of its
    /// edits after its revert and compares it with the level below. None = never.
    ///
    /// # Contract
    /// - C1: `Some(n)` makes every n-th write re-diagnose after its revert, a check being one
    ///   write (the 1st of `Some(2)` does not, the 2nd does); `None` never does through this
    ///   field.
    ///   tested: tests/verifier.rs assert_revert_every_samples_the_diagnostic_assert
    ///   tested: tests/stack.rs writes_count_for_the_sampled_reassert
    pub assert_revert_every: Option<std::num::NonZeroU32>,
    /// `CargoConfig::set_test`: `#[cfg(test)]` code of workspace members is enabled, so it is diagnosed.
    ///
    /// # Contract
    /// - C1: with the default `true` an error inside a `#[cfg(test)]` module is in the baseline;
    ///   with `false` it is not.
    ///   tested: tests/verifier.rs cfg_test_config_diagnoses_test_module
    ///   tested: tests/verifier.rs ra_config_default_diagnoses_test_code
    pub cfg_test: bool,
    /// The backend of the forks (`Stacked`, `Analyzer`, and a check, which is a one-level fork):
    /// `false` (the default) writes each level in place and reverts it from its undo log; `true`
    /// runs it on the fork Engine (D11): the base is frozen while a fork is live, a level is a
    /// layer over it, `pop` drops the top layer and `release` drops them all, so the base is
    /// never written.
    ///
    /// # Contract
    /// - C1: every differential and contract test of `Stacked`, `Verifier` and the cut-off gives
    ///   the same results on both backends, where the Engine has no revert to get wrong (a
    ///   revert fault, a re-assert of a revert and a database write of a revert have no
    ///   counterpart there), and except a level's write that panics: on the Engine it poisons
    ///   only its dropped layer and is of reach Blueprint (`Stacked` C8), where the undo log
    ///   poisons the session.
    ///   tested: tests/stack.rs a_stack_diagnoses_as_its_flat_composition
    ///   tested: tests/verifier.rs check_leaves_text_and_baseline_at_base
    ///   tested: tests/timeout.rs check_past_timeout_is_timed_out_and_session_usable
    pub engine: bool,
}

impl Default for RaConfig {
    fn default() -> Self {
        Self {
            proc_macro: true,
            threads: None,
            assert_revert: false,
            gap_detectors: true,
            assert_revert_every: None,
            cfg_test: true,
            engine: false,
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
    /// Detector and file on which it is made to panic; a test hook.
    pub(crate) detector_fault: Option<(GapDetector, RelPath)>,
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
}

/// The database write of the texts that the test hook `RaAnalyzer::with_db_write_panic` makes
/// panic.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DbWritePanic {
    /// The write of a level's edits.
    Write,
    /// The write that restores the texts of a reverted level.
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
        }
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
        self.detector_fault = fault;
        self
    }

    /// Makes every revert of a write that modified or deleted `path` restore it with its stored
    /// text plus one byte, so that the comparison of the texts must catch it.
    /// For the tests of that comparison; nothing else sets it.
    #[doc(hidden)]
    pub fn with_revert_fault(mut self, path: Option<RelPath>) -> RaAnalyzer {
        self.revert_fault = path;
        self
    }

    /// Makes the diagnostic re-assert of every sampled revert fail to run, after the texts are
    /// restored. For the tests of a revert that cannot be confirmed; nothing else sets it.
    #[doc(hidden)]
    pub fn with_reassert_fault(mut self, fault: bool) -> RaAnalyzer {
        self.reassert_fault = fault;
        self
    }

    /// Makes that database write of the texts panic on every level, before it changes the
    /// database. For the tests of a write or a revert the database cannot carry out; nothing else
    /// sets it.
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
/// - C1: a `discard` whose re-assert differs from the baseline is `RevertMismatch` and poisons
///   the session; a later `fork` is refused.
///   tested: tests/verifier.rs discard_reassert_mismatch_poisons_the_session
/// - C2: a `discard` compares the texts of the files its write modified or deleted with their
///   texts before; a difference is `RevertMismatch` and poisons the session.
///   tested: tests/stack.rs analyzer_discard_compares_texts
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
