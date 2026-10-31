//! rust-analyzer adapter of ryo-v2.
//!
//! The only crate that depends on the `ra_ap_*` stack. It implements the
//! `Analyzer` port of `ryo-v2-core`: load a workspace once, fork its in-memory
//! state per candidate, apply text edits, collect HIR diagnostics, discard the
//! fork. Nothing here runs cargo or rustc per candidate.
#![warn(missing_docs)]

mod assess;
mod check;
mod diagnostics;
mod expand;
mod fork;
mod rev_deps;
mod session;
mod unresolved;

use ryo_v2_core::app::ports::{Analyzer, AnalyzerError, Clock, Observer};
use ryo_v2_core::domain::{Edit, Finding, Scope, ScopedFindings};
use std::path::Path;
use std::sync::Arc;

pub use assess::SyntaxAssessor;
pub use fork::RaFork;
pub use session::RaSession;

/// Configuration of [`RaAnalyzer`].
#[derive(Debug, Clone)]
pub struct RaConfig {
    /// sysroot の proc-macro srv で derive 等を展開する。
    pub proc_macro: bool,
    /// 診断の並列度。None = available_parallelism。
    pub threads: Option<usize>,
    /// discard ごとに巻き戻し後の診断が baseline と一致するか検査する。
    pub assert_revert: bool,
    /// ra が診断しない未解決の型名と path 先頭を検出する (E0405/E0425/E0433)。
    pub unresolved_paths: bool,
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
            unresolved_paths: true,
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
}

impl RaAnalyzer {
    /// Builds an analyzer; `clock` times the events that go to `observer`. No check is cut off.
    pub fn new(config: RaConfig, clock: Arc<dyn Clock>, observer: Arc<dyn Observer>) -> Self {
        Self {
            config,
            clock,
            observer,
            check_timeout_ms: None,
        }
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
