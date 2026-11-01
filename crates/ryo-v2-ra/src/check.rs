//! `Verifier` for the rust-analyzer adapter: one call checks a set of edits and leaves the
//! session as it found it.

use crate::RaAnalyzer;
use crate::diagnostics::{Deadline, Detector, collect_errors_par};
use crate::fork::{self, RevertFailure};
use crate::rev_deps::scoped_files;
use crate::session::{ForkPort, RaSession};
use crate::stack;
use ra_ap_vfs::FileId;
use ryo_v2_core::app::ports::{AnalyzerError, CheckError, FileFacts, RevertAudit, Verifier};
use ryo_v2_core::domain::baseline::{restrict, same_findings};
use ryo_v2_core::domain::{
    Edit, Finding, Fingerprint, Provenance, RelPath, Scope, ScopedFindings, UnseenClass,
};
use std::collections::{BTreeMap, BTreeSet};
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
        AnalyzerError::ForkInUse => CheckError::ForkInUse,
        AnalyzerError::AlreadyApplied => CheckError::AlreadyApplied,
        AnalyzerError::Load(m) => CheckError::Load(m),
    }
}

/// How a failed revert is said to the caller of a check. A failure to revert is always about the
/// session, never about the Blueprint being checked.
fn revert_error(failed: RevertFailure) -> CheckError {
    match failed {
        RevertFailure::Mismatch(paths) => CheckError::RevertMismatch { paths },
        RevertFailure::Poisoned(msg) => CheckError::Poisoned(msg),
        RevertFailure::Failed(e) => map_error(e, &[]),
    }
}

/// The text of `id` in the base state: the record a level made before it wrote the file, or else
/// what the database holds.
fn base_text(s: &RaSession, id: FileId) -> Option<String> {
    match s.base_texts.get(&id) {
        Some(text) => Some(text.clone()),
        None => fork::db_text(&s.host, id).ok(),
    }
}

/// Diagnoses the top level for a check: under the check time limit, and with a detector that
/// panicked where it did not at the baseline failing the diagnosis.
fn diagnose_for_check(
    a: &RaAnalyzer,
    s: &RaSession,
    scope: Scope,
) -> Result<ScopedFindings, CheckError> {
    let deadline = a
        .check_timeout_ms
        .map(|ms| Deadline::after(a.clock.clone(), ms));
    let (mut found, panics) = match fork::diagnose_level(a, s, scope, deadline.as_ref()) {
        Ok(diagnosed) => diagnosed,
        Err(AnalyzerError::Cancelled) => {
            return Err(match a.check_timeout_ms {
                Some(limit_ms) if deadline.as_ref().is_some_and(Deadline::was_hit) => {
                    CheckError::TimedOut { limit_ms }
                }
                _ => CheckError::Cancelled,
            });
        }
        Err(e) => return Err(map_error(e, &[])),
    };
    // A panic the baseline had on the same file is a blind spot of the detector there before any
    // edit: named, not a failure. A new one means the file cannot be judged.
    let mut new_panic = None;
    for (detector, file) in panics {
        if s.baseline_panics.contains(&(detector, file.clone())) {
            found.unseen.insert(detector.unseen_class());
        } else if new_panic.is_none() {
            new_panic = Some((detector, file));
        }
    }
    match new_panic {
        Some((detector, file)) => Err(CheckError::DetectorPanicked {
            detector: detector.name().to_owned(),
            file,
        }),
        None => Ok(found),
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
        base_text(s, *id)
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

    /// The classes of error this analyzer, as configured, does not see.
    ///
    /// # Contract
    /// - C1: always `Borrow`, `BuildScript`, `ProcMacroCrate`, `Visibility`, `ConstEval`,
    ///   `StructLiteralPath` and `UnusedTypeParam`; and `DeriveWithoutServer` when `proc_macro`
    ///   is off, `UnresolvedPath`, `DuplicateName` and `FieldBounds` when `gap_detectors` is off,
    ///   and `TestCode` when `cfg_test` is off. `UnexpandedMacro` is never declared: a check
    ///   observes it.
    ///   tested: check::tests::ra_blind_classes_follow_the_config
    fn blind_classes(&self) -> BTreeSet<UnseenClass> {
        let mut blind = BTreeSet::from([
            UnseenClass::Borrow,
            UnseenClass::BuildScript,
            UnseenClass::ProcMacroCrate,
            UnseenClass::Visibility,
            UnseenClass::ConstEval,
            UnseenClass::StructLiteralPath,
            UnseenClass::UnusedTypeParam,
        ]);
        if !self.config.proc_macro {
            blind.insert(UnseenClass::DeriveWithoutServer);
        }
        if !self.config.gap_detectors {
            blind.extend([
                UnseenClass::UnresolvedPath,
                UnseenClass::DuplicateName,
                UnseenClass::FieldBounds,
            ]);
        }
        if !self.config.cfg_test {
            blind.insert(UnseenClass::TestCode);
        }
        blind
    }

    /// The analyzer, its configuration and the fingerprint of each workspace file's database
    /// text, of the manifests and of the lock file.
    ///
    /// # Contract
    /// - C1: the analyzer is `rust-analyzer` at the release the `ra_ap_*` crates are pinned to;
    ///   the config lists `proc_macro`, `cfg_test`, `gap_detectors`, `check_timeout_ms`
    ///   (`none` when no limit is set) and `toolchain` (the first line of `rustc --version` in
    ///   the root, `unknown` if it could not be run; it stands for that toolchain's sysroot, whose
    ///   path is not recorded); the files are the indexed workspace files whose text can be read,
    ///   plus the root `Cargo.toml`, each member's `Cargo.toml` and the root `Cargo.lock` as read
    ///   at load (one that could not be read is left out), so a manifest or lock change moves
    ///   the workspace fingerprint; the `gap_detectors` entry is the switch of every gap detector.
    ///   A file's database text is its bytes on disk at load (line endings are not normalized),
    ///   so `prepare_write` can compare the fingerprint with that of the file's current text.
    ///   tested: check::tests::analyzer_version_is_the_manifest_pin
    ///   tested: tests/verifier.rs provenance_fingerprints_are_of_the_disk_text
    ///   tested: tests/verifier.rs provenance_records_the_gap_detector_switch
    ///   tested: tests/verifier.rs provenance_fingerprints_manifests_lock_and_toolchain
    ///   tested: tests/verifier.rs provenance_fingerprints_each_member_manifest
    fn provenance(&self, s: &RaSession) -> Provenance {
        let files: BTreeMap<RelPath, Fingerprint> = s
            .files
            .iter()
            .filter_map(|(id, path)| {
                let text = base_text(s, *id)?;
                Some((path.clone(), Fingerprint::of(text.as_bytes())))
            })
            .collect();
        let files = files
            .into_iter()
            .chain(s.load_inputs.manifests.iter().cloned())
            .collect();
        let config = BTreeMap::from([
            ("proc_macro".to_owned(), self.config.proc_macro.to_string()),
            ("cfg_test".to_owned(), self.config.cfg_test.to_string()),
            (
                "gap_detectors".to_owned(),
                self.config.gap_detectors.to_string(),
            ),
            (
                "check_timeout_ms".to_owned(),
                self.check_timeout_ms
                    .map_or_else(|| "none".to_owned(), |ms| ms.to_string()),
            ),
            ("toolchain".to_owned(), s.load_inputs.toolchain.clone()),
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
    ///   tested: tests/stack.rs text_is_the_base_after_a_poisoned_check
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
    /// - C5: a gap detector that panicked on a file of the scope in this check's own diagnostics,
    ///   where it did not panic at baseline, makes the result `CheckError::DetectorPanicked` (the
    ///   first by detector and file), with the edits still reverted and the session usable; one
    ///   that panicked on the same file at baseline too does not fail the check and names the
    ///   detector's class (`GapDetector::unseen_class`) in the findings' `unseen`.
    ///   tested: tests/unresolved.rs detector_panic_is_contained_per_file
    ///   tested: tests/gap_detectors.rs each_detector_panic_is_contained_on_its_own
    /// - C6: `RevertMismatch` and `Poisoned` win over every other error of the check: an `Edit`,
    ///   a `TimedOut`, a `Panicked` and a `DetectorPanicked`.
    ///   tested: tests/verifier.rs session_errors_win_over_a_detector_panic
    ///   tested: tests/stack.rs a_session_error_of_the_release_wins_in_a_check
    /// - C7: with no edit the check diagnoses no file at `Scope::Affected` and names no unseen
    ///   class.
    ///   tested: tests/stack.rs an_empty_check_diagnoses_nothing
    /// - C8: a check whose edits are refused records no check or revert event and pushes no
    ///   level.
    ///   tested: tests/stack.rs a_refused_check_reverts_nothing
    fn check(
        &self,
        s: &mut RaSession,
        edits: &[Edit],
        scope: Scope,
    ) -> Result<ScopedFindings, CheckError> {
        stack::require_free(s)?;
        let mut edited: Vec<RelPath> = edits.iter().map(|e| e.path.clone()).collect();
        edited.sort();
        edited.dedup();
        s.edited_since_audit.extend(edited.iter().cloned());

        s.write_count = s.write_count.wrapping_add(1);
        let sampled = s.assert_revert
            || self
                .config
                .assert_revert_every
                .is_some_and(|n| s.write_count.is_multiple_of(n.get()));

        // A check is the one-level case of a fork: write, diagnose, release. The release always
        // runs, and its failure, always about the session, wins; then the write's; then the
        // diagnosis's.
        stack::open_fork(s, ForkPort::Stacked);
        let outcome = fork::push_level(self, s, edits, sampled)
            .map_err(|e| map_error(e, &edited))
            .and_then(|()| diagnose_for_check(self, s, scope));
        fork::release_levels(self, s).map_err(revert_error)?;
        outcome
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
        stack::require_free(s)?;
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
            &s.files,
            s.threads,
            s.gap_detectors,
            // An audit is never cut off.
            None,
            Detector {
                fault: self.detector_fault.as_ref(),
                panics: &s.detector_panics,
            },
        )
        .map_err(|e| map_error(e, &edited))?
        .findings;
        let paths: Vec<RelPath> = scope.iter().map(|(_, r)| r.clone()).collect();
        if same_findings(&found, &restrict(&s.baseline, &paths)) {
            Ok(())
        } else {
            s.poisoned = Some("diagnostics after revert differ from the baseline".to_owned());
            Err(CheckError::RevertMismatch { paths: edited })
        }
    }
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

    #[test]
    fn ra_blind_classes_follow_the_config() {
        use crate::RaConfig;
        use ryo_v2_core::infra::clock::MonotonicClock;
        use ryo_v2_core::infra::observer::RecordingObserver;
        use std::sync::Arc;

        let always = BTreeSet::from([
            UnseenClass::Borrow,
            UnseenClass::BuildScript,
            UnseenClass::ProcMacroCrate,
            UnseenClass::Visibility,
            UnseenClass::ConstEval,
            UnseenClass::StructLiteralPath,
            UnseenClass::UnusedTypeParam,
        ]);
        for mask in 0..8u8 {
            let (proc_macro, gap_detectors, cfg_test) =
                (mask & 1 != 0, mask & 2 != 0, mask & 4 != 0);
            let a = RaAnalyzer::new(
                RaConfig {
                    proc_macro,
                    gap_detectors,
                    cfg_test,
                    ..RaConfig::default()
                },
                Arc::new(MonotonicClock::new()),
                Arc::new(RecordingObserver::default()),
            );
            let mut want = always.clone();
            if !proc_macro {
                want.insert(UnseenClass::DeriveWithoutServer);
            }
            if !gap_detectors {
                want.extend([
                    UnseenClass::UnresolvedPath,
                    UnseenClass::DuplicateName,
                    UnseenClass::FieldBounds,
                ]);
            }
            if !cfg_test {
                want.insert(UnseenClass::TestCode);
            }
            assert_eq!(a.blind_classes(), want, "{mask:03b}");
            assert!(!a.blind_classes().contains(&UnseenClass::UnexpandedMacro));
        }
    }

    #[test]
    fn map_error_keeps_fork_in_use_already_applied_and_load_apart() {
        assert_eq!(
            map_error(AnalyzerError::ForkInUse, &[]),
            CheckError::ForkInUse
        );
        assert_eq!(
            map_error(AnalyzerError::AlreadyApplied, &[]),
            CheckError::AlreadyApplied
        );
        assert_eq!(
            map_error(AnalyzerError::Load("l".to_owned()), &[]),
            CheckError::Load("l".to_owned())
        );
    }
}
