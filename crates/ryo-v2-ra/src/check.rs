//! `Verifier` for the rust-analyzer adapter: one call checks a set of edits and leaves the
//! session as it found it.

use crate::RaAnalyzer;
use crate::diagnostics::{Deadline, Detector, collect_errors_par};
use crate::fork;
use crate::rev_deps::scoped_files;
use crate::session::RaSession;
use ryo_v2_core::app::ports::{
    Analyzer, AnalyzerError, CheckError, FileFacts, RevertAudit, Verifier,
};
use ryo_v2_core::domain::baseline::{restrict, same_findings};
use ryo_v2_core::domain::text::revert_intact;
use ryo_v2_core::domain::{Edit, Finding, Fingerprint, Provenance, RelPath, Scope, ScopedFindings};
use std::collections::BTreeMap;
use std::path::Path;

/// The rust-analyzer release the adapter is built on; equals the `=` pin of the `ra_ap_*` crates
/// in the workspace manifest.
pub(crate) const ANALYZER_VERSION: &str = "0.0.358";

/// Fingerprints of the database text of `paths` that are workspace files.
pub(crate) fn fingerprints(session: &RaSession, paths: &[RelPath]) -> Vec<(RelPath, Fingerprint)> {
    paths
        .iter()
        .filter_map(|p| {
            let (id, _) = session.files.iter().find(|(_, r)| r == p)?;
            let text = fork::db_text(&session.host, *id).ok()?;
            Some((p.clone(), Fingerprint::of(text.as_bytes())))
        })
        .collect()
}

pub(crate) fn map_error(e: AnalyzerError, edited: &[RelPath]) -> CheckError {
    match e {
        AnalyzerError::Edit { path, reason } => CheckError::Edit { path, reason },
        AnalyzerError::Cancelled => CheckError::Cancelled,
        AnalyzerError::Panicked(m) => CheckError::Panicked(m),
        AnalyzerError::RevertMismatch => CheckError::RevertMismatch {
            paths: edited.to_vec(),
        },
        other => CheckError::Poisoned(other.to_string()),
    }
}

/// The adapter's `Verifier`.
///
/// # Contract
/// - C1: after `check`, whatever its result, `text` of every edited path and `baseline` equal
///   what they were before, and a second `check` on the same session works.
///   tested: tests/verifier.rs check_leaves_text_and_baseline_at_base
/// - C2: an edit of an existing path that is not an indexed `.rs` file (a manifest) is
///   `CheckError::Edit`, and the session stays usable.
///   tested: tests/verifier.rs check_of_unindexed_existing_path_is_edit_error
impl Verifier for RaAnalyzer {
    type Session = RaSession;

    /// The text the database holds for `path`; `None` when `path` is not an indexed workspace
    /// file. Never reads the disk.
    ///
    /// # Contract
    /// - C1: a path outside the indexed files (a manifest) gives `None`; an indexed file gives
    ///   its database text.
    ///   tested: tests/verifier.rs check_leaves_text_and_baseline_at_base
    ///   tested: tests/verifier.rs check_of_unindexed_existing_path_is_edit_error
    fn text(&self, s: &RaSession, path: &RelPath) -> Option<String> {
        let (id, _) = s.files.iter().find(|(_, r)| r == path)?;
        fork::db_text(&s.host, *id).ok()
    }

    /// Facts of `path` from the one `cargo metadata` call at load.
    ///
    /// # Contract
    /// - C1: a build-script source has `is_build_script`; a file under a proc-macro member's
    ///   dir has `in_proc_macro_crate`; any other file has neither.
    ///   tested: tests/verifier.rs facts_mark_build_script_and_proc_macro_crate
    fn facts(&self, s: &RaSession, path: &RelPath) -> FileFacts {
        let p = Path::new(path.as_str());
        FileFacts {
            is_build_script: s.path_facts.build_scripts.iter().any(|b| b == p),
            in_proc_macro_crate: s
                .path_facts
                .proc_macro_dirs
                .iter()
                .any(|d| p.starts_with(d)),
        }
    }

    fn baseline(&self, s: &RaSession) -> Vec<Finding> {
        s.baseline.clone()
    }

    /// The analyzer, its configuration and the fingerprint of each workspace file's database
    /// text.
    ///
    /// # Contract
    /// - C1: the analyzer is `rust-analyzer` at the release the `ra_ap_*` crates are pinned to;
    ///   the config lists `proc_macro`, `cfg_test`, `unresolved_paths` and `check_timeout_ms`
    ///   (`none` when no limit is set); the files are the indexed workspace files whose text can
    ///   be read.
    ///   tested: check::tests::analyzer_version_is_the_manifest_pin
    fn provenance(&self, s: &RaSession) -> Provenance {
        let files: BTreeMap<RelPath, Fingerprint> = s
            .files
            .iter()
            .filter_map(|(id, path)| {
                let text = fork::db_text(&s.host, *id).ok()?;
                Some((path.clone(), Fingerprint::of(text.as_bytes())))
            })
            .collect();
        let config = BTreeMap::from([
            ("proc_macro".to_owned(), self.config.proc_macro.to_string()),
            ("cfg_test".to_owned(), self.config.cfg_test.to_string()),
            (
                "unresolved_paths".to_owned(),
                self.config.unresolved_paths.to_string(),
            ),
            (
                "check_timeout_ms".to_owned(),
                self.check_timeout_ms
                    .map_or_else(|| "none".to_owned(), |ms| ms.to_string()),
            ),
        ]);
        Provenance::new("rust-analyzer", ANALYZER_VERSION, config, files)
            .unwrap_or_else(|_| Provenance::unrecorded())
    }

    /// Applies `edits` to the in-memory state, diagnoses `scope`, and reverts.
    ///
    /// # Contract
    /// - C1: the text fingerprints of the edited files are compared before and after; a
    ///   difference poisons the session and is `RevertMismatch`.
    ///   tested: tests/verifier.rs check_leaves_text_and_baseline_at_base
    /// - C2: the diagnostic re-assert runs on every n-th call when `assert_revert_every` is
    ///   `Some(n)`.
    ///   tested: tests/verifier.rs assert_revert_every_samples_the_diagnostic_assert
    /// - C3: with a cut-off set (`with_check_timeout`) a diagnostics step that is still running
    ///   when the limit is reached ends in `CheckError::TimedOut`; the edits are still reverted,
    ///   the text fingerprints still compared, and the session stays usable. The limit is
    ///   looked at before each file, so one slow file can overrun it.
    ///   tested: tests/timeout.rs check_past_timeout_is_timed_out_and_session_usable
    /// - C4: a diagnostic re-assert that differs from the baseline is `RevertMismatch` and
    ///   poisons the session; the next call is `Poisoned`.
    ///   tested: tests/verifier.rs reassert_mismatch_poisons_the_session
    fn check(
        &self,
        s: &mut RaSession,
        edits: &[Edit],
        scope: Scope,
    ) -> Result<ScopedFindings, CheckError> {
        if let Some(msg) = &s.poisoned {
            return Err(CheckError::Poisoned(msg.clone()));
        }
        let mut edited: Vec<RelPath> = edits.iter().map(|e| e.path.clone()).collect();
        edited.sort();
        edited.dedup();
        s.edited_since_audit.extend(edited.iter().cloned());
        let before = fingerprints(s, &edited);

        s.check_count = s.check_count.wrapping_add(1);
        let sampled = self
            .config
            .assert_revert_every
            .is_some_and(|n| s.check_count.is_multiple_of(n.get()));
        let saved_assert = s.assert_revert;
        if sampled {
            s.assert_revert = true;
        }
        let (outcome, timed_out) = run_fork(self, s, edits, scope);
        s.assert_revert = saved_assert;

        let after = fingerprints(s, &edited);
        if let Err(paths) = revert_intact(&before, &after) {
            s.poisoned = Some(format!(
                "text of {} file(s) differs after revert",
                paths.len()
            ));
            return Err(CheckError::RevertMismatch { paths });
        }
        match (outcome, self.check_timeout_ms) {
            (Err(AnalyzerError::Cancelled), Some(limit_ms)) if timed_out => {
                Err(CheckError::TimedOut { limit_ms })
            }
            (outcome, _) => outcome.map_err(|e| map_error(e, &edited)),
        }
    }
}

/// Re-diagnosing the files edited since the last audit, to catch a revert that left a trace the
/// text fingerprints cannot show.
///
/// # Contract
/// - C1: after checks whose reverts were clean, `audit_revert` is `Ok` and forgets the paths
///   it audited; with nothing recorded it is `Ok` and diagnoses nothing.
///   tested: tests/timeout.rs audit_revert_is_ok_after_checks_and_refuses_poisoned
/// - C2: a session already poisoned gives `CheckError::Poisoned`.
///   tested: tests/timeout.rs audit_revert_is_ok_after_checks_and_refuses_poisoned
impl RevertAudit for RaAnalyzer {
    fn audit_revert(&self, s: &mut RaSession) -> Result<(), CheckError> {
        if let Some(msg) = &s.poisoned {
            return Err(CheckError::Poisoned(msg.clone()));
        }
        if s.edited_since_audit.is_empty() {
            return Ok(());
        }
        let edited: Vec<RelPath> = std::mem::take(&mut s.edited_since_audit)
            .into_iter()
            .collect();
        let refs: Vec<&RelPath> = edited.iter().collect();
        let scope = scoped_files(&s.files, &s.rev_deps, &refs);
        let found = collect_errors_par(
            &s.host,
            &scope,
            s.threads,
            s.unresolved_paths,
            // An audit is never cut off.
            None,
            Detector {
                fault: self.detector_fault.as_ref(),
                panics: &s.detector_panics,
            },
        )
        .map_err(|e| map_error(e, &edited))?;
        let paths: Vec<RelPath> = scope.iter().map(|(_, r)| r.clone()).collect();
        if same_findings(&found, &restrict(&s.baseline, &paths)) {
            Ok(())
        } else {
            s.poisoned = Some("diagnostics after revert differ from the baseline".to_owned());
            Err(CheckError::RevertMismatch { paths: edited })
        }
    }
}

/// fork, apply, diagnostics, discard; discard always runs. The `Err` that wins: apply, then
/// diagnostics, then discard (the same as `app::verify`). The flag says that the cut-off ended
/// the diagnostics.
fn run_fork(
    a: &RaAnalyzer,
    s: &mut RaSession,
    edits: &[Edit],
    scope: Scope,
) -> (Result<ScopedFindings, AnalyzerError>, bool) {
    let mut f = match a.fork(s) {
        Ok(f) => f,
        Err(e) => return (Err(e), false),
    };
    let applied = a.apply(s, &mut f, edits);
    let mut timed_out = false;
    let checked = match applied {
        Ok(()) => {
            s.deadline = a
                .check_timeout_ms
                .map(|ms| Deadline::after(a.clock.clone(), ms));
            let r = a.diagnostics(s, &f, scope);
            timed_out = s.deadline.take().is_some_and(|d| d.was_hit());
            Some(r)
        }
        Err(_) => None,
    };
    let discarded = a.discard(s, f);
    let outcome = applied.and_then(|()| {
        let found = checked.unwrap_or_else(|| Ok(ScopedFindings::default()))?;
        discarded?;
        Ok(found)
    });
    (outcome, timed_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyzer_version_is_the_manifest_pin() {
        let manifest = include_str!("../../../Cargo.toml");
        let pin = format!("ra_ap_ide = \"={ANALYZER_VERSION}\"");
        assert!(manifest.contains(&pin), "{pin}");
    }
}
