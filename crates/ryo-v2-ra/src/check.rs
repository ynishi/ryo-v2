//! `Verifier` for the rust-analyzer adapter: one call checks a set of edits and leaves the
//! session as it found it.

use crate::RaAnalyzer;
use crate::diagnostics::{Deadline, Detector, GapDetector, collect_errors_par};
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
        AnalyzerError::Poisoned(m) => CheckError::Poisoned(m),
        AnalyzerError::RevertMismatch => CheckError::RevertMismatch {
            paths: edited.to_vec(),
        },
        AnalyzerError::ForkInUse => CheckError::ForkInUse,
        AnalyzerError::AlreadyApplied => CheckError::AlreadyApplied,
        AnalyzerError::Load(m) => CheckError::Load(m),
    }
}

/// How a failed revert is said to the caller of a check. A failure to revert is always about the
/// session, never about the Blueprint being checked: it has poisoned the session.
pub(crate) fn revert_error(failed: RevertFailure) -> CheckError {
    match failed {
        RevertFailure::Mismatch(paths) => CheckError::RevertMismatch { paths },
        RevertFailure::Poisoned(msg) => CheckError::Poisoned(msg),
    }
}

/// The text of `path` in the base state. `Ok(None)` means only that `path` is not a file of the
/// base (not a file of the session); `Err` is a read that failed.
pub(crate) fn base_text(
    a: &RaAnalyzer,
    s: &RaSession,
    path: &RelPath,
) -> Result<Option<String>, AnalyzerError> {
    match s.files.iter().find(|(_, r)| r == path) {
        Some((id, _)) => base_text_at(a, s, *id).map(Some),
        None => Ok(None),
    }
}

/// The text of the session file `id` in the base state: the record a level made before it wrote
/// the file, or else what the database holds. A read that fails (the database read panics, or the
/// test hook `RaAnalyzer::with_db_read_fault` names the file) is the error.
pub(crate) fn base_text_at(
    a: &RaAnalyzer,
    s: &RaSession,
    id: FileId,
) -> Result<String, AnalyzerError> {
    if let Some(fault) = &a.db_read_fault
        && s.files.iter().any(|(i, r)| *i == id && r == fault)
    {
        return Err(AnalyzerError::Panicked(format!(
            "database read fault injected on {fault}"
        )));
    }
    match s.base_texts.get(&id) {
        Some(text) => Ok(text.clone()),
        None => fork::base_db_text(s, id),
    }
}

/// Diagnoses the top level as a check does, and as `Stacked::diagnose` does: under the check time
/// limit, and with a detector that panicked where it did not at the baseline failing the
/// diagnosis.
pub(crate) fn diagnose_for_check(
    a: &RaAnalyzer,
    s: &RaSession,
    scope: Scope,
) -> Result<ScopedFindings, CheckError> {
    let deadline = a
        .check_timeout_ms
        .map(|ms| Deadline::after(a.clock.clone(), ms));
    finish_check(
        a,
        s,
        deadline.as_ref(),
        fork::diagnose_level(a, s, scope, deadline.as_ref()),
    )
}

impl RaAnalyzer {
    /// Diagnoses every file of the session on `db` as a check at `Scope::Workspace` diagnoses its
    /// level: the same collect, cut-off and detector rules, so with `db` holding the state of a
    /// check's edits the result is that check's. For the fork Engine's differential gate (D11,
    /// stage 2): `db` is a handle of a layer over the session's own frozen database (lent with
    /// [`RaSession::lend_database`]), with modify-only edits, so the session's files are the
    /// layer's. Not a port: the Engine is put behind the ports at stage 3. The clone of `db` it
    /// makes lives only for the call.
    ///
    /// # Contract
    /// - C1: on a layer whose chain wrote modify-only edits, the result equals `check` of the
    ///   composed edits on a twin session of the same workspace (findings, files, `unseen_at`).
    ///   tested: ryo-v2-engine tests/v2.rs v2_layers_diagnose_as_their_flat_check
    #[doc(hidden)]
    pub fn diagnose_database(
        &self,
        s: &RaSession,
        db: &ra_ap_ide_db::RootDatabase,
    ) -> Result<ScopedFindings, CheckError> {
        let host = ra_ap_ide::AnalysisHost::with_database(db.clone());
        let deadline = self
            .check_timeout_ms
            .map(|ms| Deadline::after(self.clock.clone(), ms));
        let diagnosed = fork::diagnose_files(self, s, &host, &s.files, &s.files, deadline.as_ref());
        finish_check(self, s, deadline.as_ref(), diagnosed)
    }
}

/// The verdict of a check's diagnosis: the cut-off as `TimedOut`, and a detector that panicked
/// where it did not at the baseline as `DetectorPanicked` (one that did is a blind spot named in
/// `unseen_at`).
pub(crate) fn finish_check(
    a: &RaAnalyzer,
    s: &RaSession,
    deadline: Option<&Deadline>,
    diagnosed: Result<fork::Diagnosed, AnalyzerError>,
) -> Result<ScopedFindings, CheckError> {
    finish_diagnosis(a.check_timeout_ms, &s.baseline_panics, deadline, diagnosed)
}

/// [`finish_check`] given the time limit and the baseline's detector panics instead of the
/// analyzer and the session, for a diagnosis on a worker of `Forks`.
pub(crate) fn finish_diagnosis(
    check_timeout_ms: Option<u64>,
    baseline_panics: &BTreeSet<(GapDetector, RelPath)>,
    deadline: Option<&Deadline>,
    diagnosed: Result<fork::Diagnosed, AnalyzerError>,
) -> Result<ScopedFindings, CheckError> {
    let (mut found, panics) = match diagnosed {
        Ok(diagnosed) => diagnosed,
        Err(AnalyzerError::Cancelled) => {
            return Err(match check_timeout_ms {
                Some(limit_ms) if deadline.is_some_and(Deadline::was_hit) => {
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
        if baseline_panics.contains(&(detector, file.clone())) {
            found
                .unseen_at
                .entry(detector.unseen_class())
                .or_default()
                .insert(file);
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
        // A read failure is not reported here: the port's reads have no `Result` (AR-014, tasks
        // 10 and 11).
        base_text(self, s, path).ok().flatten()
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
                // A read failure is not reported here: the port's reads have no `Result` (AR-014,
                // tasks 10 and 11).
                let text = base_text_at(self, s, *id).ok()?;
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

    /// Writes `edits` into a layer over the frozen base, diagnoses `scope`, and drops the layer.
    ///
    /// # Contract
    /// - C1: the base is never written: after a check every text, the provenance and the
    ///   baseline are the base's, whatever the write-back hooks would have done.
    ///   tested: tests/verifier.rs check_leaves_text_and_baseline_at_base
    ///   tested: tests/stack.rs text_is_the_base_after_a_poisoned_check
    /// - C2: the isolation of the base (I2) is checked on every n-th write when
    ///   `assert_revert_every` is `Some(n)`, a check being one write.
    ///   tested: tests/verifier.rs assert_revert_every_samples_the_diagnostic_assert
    ///   tested: tests/stack.rs writes_count_for_the_sampled_isolation
    /// - C3: with a cut-off set (`with_check_timeout`) a diagnostics step that is still running
    ///   when the limit is reached ends in `CheckError::TimedOut`; the layer is still dropped and
    ///   the session stays usable. The limit is looked at before each file, so one slow file can
    ///   overrun it.
    ///   tested: tests/timeout.rs check_past_timeout_is_timed_out_and_session_usable
    /// - C4: a check re-asserts no revert: with `assert_revert` it checks the isolation of the
    ///   base, which a detector fault does not touch, so a new detector panic decides (C5) and the
    ///   session stays usable. The flat reference check's re-assert is `reference_check` C3.
    ///   tested: tests/verifier.rs a_sampled_check_reasserts_no_revert
    /// - C5: a gap detector that panicked on a file of the scope in this check's own diagnostics,
    ///   where it did not panic at baseline, makes the result `CheckError::DetectorPanicked` (the
    ///   first by detector and file), with the layer still dropped and the session usable; one
    ///   that panicked on the same file at baseline too does not fail the check and names the
    ///   detector's class (`GapDetector::unseen_class`) in the findings' `unseen_at`, with that
    ///   file.
    ///   tested: tests/unresolved.rs detector_panic_is_contained_per_file
    ///   tested: tests/gap_detectors.rs each_detector_panic_is_contained_on_its_own
    ///   tested: tests/stack.rs a_diagnosis_names_where_it_observed_each_unseen_class
    /// - C6: `RevertMismatch` and `Poisoned` win over every other error of the check: an `Edit`,
    ///   a `TimedOut`, a `Panicked` and a `DetectorPanicked`; so they do in the flat reference
    ///   check, whose write-back can fail.
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
        // A check is the one-level case of a fork: write, diagnose, release. The release always
        // runs, and its failure, always about the session, wins; then the write's; then the
        // diagnosis's.
        stack::open_fork(s, ForkPort::Stacked).map_err(|e| map_error(e, &[]))?;
        let outcome =
            stack::write_level(self, s, edits).and_then(|()| diagnose_for_check(self, s, scope));
        crate::engine::release_levels(self, s).map_err(revert_error)?;
        outcome
    }
}

impl RaAnalyzer {
    /// The flat reference check (D11, R1 of stage 4; `flat`): a check of `edits` that runs no
    /// line of the layer code. With no fork live it writes the edits into the session's own
    /// database with rust-analyzer's setters (`apply_change`), diagnoses at `scope` under the
    /// check's cut-off and detector rules, and writes the base texts and source roots back; the
    /// texts of the files it modified or deleted are then compared with their base texts, and
    /// with `assert_revert` the scope of the edited paths is diagnosed again and compared with
    /// the baseline there (the one-level re-assert). A difference poisons the session and is
    /// `RevertMismatch`, so a write-back that leaves a trace the texts do not show cannot shift
    /// the base under the next check. It is not a write of the sampling (the write count does
    /// not move); its paths are recorded for the revert audit, since it writes the base. The
    /// differential tests compare the Engine's checks and jobs with it.
    ///
    /// # Contract
    /// - C1: no layer exists during it and the fork counters of the database (shared writes,
    ///   skipped and refused writes) are as before it; its result equals `check` of the same
    ///   edits, and the session is left at its base.
    ///   tested: tests/stack.rs reference_check_runs_no_layer_code
    /// - C2: at `Scope::Workspace` its files and findings are those of a fresh load of the state
    ///   the edits give (the workspace with the state written to disk, loaded by a new session of
    ///   the same configuration): the anchor of the flat side of every differential test (R1).
    ///   tested: ryo-v2-oracle tests/search_recall.rs the_reference_check_is_a_fresh_load
    /// - C3: with `assert_revert` a write-back whose texts are back but whose re-assert differs
    ///   from the baseline is `RevertMismatch` of the edited paths and poisons the session, and
    ///   a re-assert that cannot run poisons it too (`Poisoned`); a write-back that leaves another
    ///   text is `RevertMismatch` of that path whether or not the re-assert is on. While poisoned,
    ///   `Verifier::text` still describes the base.
    ///   tested: tests/stack.rs the_flat_reference_reasserts_its_level
    ///   tested: tests/stack.rs text_is_the_base_after_a_poisoned_check
    #[doc(hidden)]
    pub fn reference_check(
        &self,
        s: &mut RaSession,
        edits: &[Edit],
        scope: Scope,
    ) -> Result<ScopedFindings, CheckError> {
        crate::flat::reference_check(self, s, edits, scope)
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
/// - C3: an audit whose diagnosis cannot run returns that failure and keeps every path it was
///   to audit, so the next audit diagnoses them; an audit that runs to its end, clean or not,
///   forgets the paths it audited.
///   tested: tests/timeout.rs a_failed_audit_keeps_its_paths
/// - C4: the audit first checks that the base was never written (I2): a write the frozen base
///   refused, at any time since load, poisons the session and is `RevertMismatch` of the paths;
///   then it re-diagnoses, on the same cadence.
///   tested: tests/timeout.rs audit_revert_is_ok_after_checks_and_refuses_poisoned
impl RevertAudit for RaAnalyzer {
    fn audit_revert(&self, s: &mut RaSession) -> Result<(), CheckError> {
        stack::require_free(s)?;
        if s.edited_since_audit.is_empty() {
            return Ok(());
        }
        if ryo_v2_engine::counters(s.host.raw_database()).refused > 0 {
            s.poisoned = Some("a layer wrote the base (I2)".to_owned());
            return Err(CheckError::RevertMismatch {
                paths: s.edited_since_audit.iter().cloned().collect(),
            });
        }
        // The record is kept until the diagnosis has run to its end: an audit that cannot run
        // leaves every path for the next one.
        let edited: Vec<RelPath> = s.edited_since_audit.iter().cloned().collect();
        let refs: Vec<&RelPath> = edited.iter().collect();
        let scope = scoped_files(&s.files, &s.rev_deps, &refs);
        let collected = if self.audit_fault {
            Err(AnalyzerError::Panicked(
                "injected revert audit fault".to_owned(),
            ))
        } else {
            collect_errors_par(
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
                    probe: self.thread_count.as_deref(),
                    times: self.file_times.as_deref(),
                },
            )
        };
        let found = collected.map_err(|e| map_error(e, &edited))?.findings;
        // `edited` is the whole record, and nothing writes to it while the audit holds the session.
        s.edited_since_audit.clear();
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
    fn map_error_keeps_the_poison_apart_from_a_panic() {
        assert_eq!(
            map_error(AnalyzerError::Poisoned("p".to_owned()), &[]),
            CheckError::Poisoned("p".to_owned())
        );
        assert_eq!(
            map_error(AnalyzerError::Panicked("p".to_owned()), &[]),
            CheckError::Panicked("p".to_owned())
        );
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
