//! rust-analyzer adapter of ryo-v2.
//!
//! The only crate that depends on the `ra_ap_*` stack. It implements the
//! `Analyzer` port of `ryo-v2-core`: load a workspace once, fork its in-memory
//! state per candidate, apply text edits, collect HIR diagnostics, discard the
//! fork. Nothing here runs cargo or rustc per candidate.

mod assess;
mod check;
mod diagnostics;
mod duplicate;
mod expand;
mod field_bounds;
mod fork;
mod rev_deps;
mod session;
mod unresolved;

use ryo_v2_core::app::ports::{Analyzer, AnalyzerError, Clock, Observer};
use ryo_v2_core::domain::{Edit, Finding, RelPath, Scope, ScopedFindings};
use std::path::Path;
use std::sync::Arc;

pub use assess::SyntaxAssessor;
pub use diagnostics::GapDetector;
pub use fork::RaFork;
pub use session::{DetectorPanic, ExpanderPanic, RaSession};

/// Configuration of [`RaAnalyzer`].
#[derive(Debug, Clone)]
pub struct RaConfig {
    /// sysroot の proc-macro srv で derive 等を展開する。
    pub proc_macro: bool,
    /// 診断の並列度。None = available_parallelism。
    pub threads: Option<usize>,
    /// discard ごとに巻き戻し後の診断が baseline と一致するか検査する。
    pub assert_revert: bool,
    /// ra が診断しない error の detector を走らせる: 未解決の型名と path 先頭 (E0405/E0425/E0433)、
    /// item と `use` が同じ module で同じ名前を定義する (E0255)、derive した impl と手書きの
    /// `Copy` impl が field に求める trait (E0204/E0277/E0369)。全ての [`GapDetector`] を一斉に切り替える。以前の名前は `unresolved_paths`(最初の detector の名前)。
    pub gap_detectors: bool,
    /// `Verifier::check` が n 回に 1 回、巻き戻し後の診断を baseline と突き合わせる。None = 突き合わせない。
    ///
    /// # Contract
    /// - C1: `Some(n)` makes every n-th `Verifier::check` re-diagnose after the revert (the 1st
    ///   of `Some(2)` does not, the 2nd does); `None` never does through this field.
    ///   tested: tests/verifier.rs assert_revert_every_samples_the_diagnostic_assert
    pub assert_revert_every: Option<std::num::NonZeroU32>,
    /// `CargoConfig::set_test`: `#[cfg(test)]` code of workspace members is enabled, so it is diagnosed.
    ///
    /// # Contract
    /// - C1: with `true` an error inside a `#[cfg(test)]` module is in the baseline; with the
    ///   default `false` it is not.
    ///   tested: tests/verifier.rs cfg_test_config_diagnoses_test_module
    pub cfg_test: bool,
}

impl Default for RaConfig {
    fn default() -> Self {
        Self {
            proc_macro: true,
            threads: None,
            assert_revert: false,
            gap_detectors: true,
            assert_revert_every: None,
            cfg_test: false,
        }
    }
}

/// The rust-analyzer implementation of the `Analyzer` port.
pub struct RaAnalyzer {
    pub(crate) config: RaConfig,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) observer: Arc<dyn Observer>,
    /// Cut-off of the diagnostics of one `Verifier::check`, in ms. A field of the analyzer and
    /// not of `RaConfig`, so literals of `RaConfig` elsewhere keep compiling.
    pub(crate) check_timeout_ms: Option<u64>,
    /// Detector and file on which it is made to panic; a test hook.
    pub(crate) detector_fault: Option<(GapDetector, RelPath)>,
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
        }
    }

    /// Makes that detector panic on that file in every collect this analyzer runs.
    /// For the tests of the containment and of the re-assert; nothing else sets it.
    #[doc(hidden)]
    pub fn with_detector_fault(mut self, fault: Option<(GapDetector, RelPath)>) -> RaAnalyzer {
        self.detector_fault = fault;
        self
    }

    /// Cut-off of the diagnostics of one `Verifier::check`; `None` (the default of `new`) = no
    /// cut-off. Baseline, revert assert, revert audit and expansion are never cut off.
    ///
    /// # Contract
    /// - C1: with `Some(limit_ms)` a `check` whose diagnostics reach the limit ends in
    ///   `CheckError::TimedOut`; with `None` it never does.
    ///   tested: tests/timeout.rs check_past_timeout_is_timed_out_and_session_usable
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
        fork::discard(self, session, fork)
    }
}
