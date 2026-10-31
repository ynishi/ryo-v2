//! `Verifier` for the rust-analyzer adapter: one call checks a set of edits and leaves the
//! session as it found it.

use crate::RaAnalyzer;
use crate::fork;
use crate::session::RaSession;
use ryo_v2_core::app::ports::{Analyzer, AnalyzerError, CheckError, FileFacts, Verifier};
use ryo_v2_core::domain::text::revert_intact;
use ryo_v2_core::domain::{Edit, Finding, Fingerprint, RelPath, Scope, ScopedFindings};
use std::path::Path;

/// Fingerprints of the database text of `paths` that are workspace files.
fn fingerprints(session: &RaSession, paths: &[RelPath]) -> Vec<(RelPath, Fingerprint)> {
    paths
        .iter()
        .filter_map(|p| {
            let (id, _) = session.files.iter().find(|(_, r)| r == p)?;
            let text = fork::db_text(&session.host, *id).ok()?;
            Some((p.clone(), Fingerprint::of(text.as_bytes())))
        })
        .collect()
}

fn map_error(e: AnalyzerError, edited: &[RelPath]) -> CheckError {
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

    /// Applies `edits` to the in-memory state, diagnoses `scope`, and reverts.
    ///
    /// # Contract
    /// - C1: the text fingerprints of the edited files are compared before and after; a
    ///   difference poisons the session and is `RevertMismatch`.
    ///   tested: tests/verifier.rs check_leaves_text_and_baseline_at_base
    /// - C2: the diagnostic re-assert runs on every n-th call when `assert_revert_every` is
    ///   `Some(n)`.
    ///   tested: tests/verifier.rs assert_revert_every_samples_the_diagnostic_assert
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
        let outcome = run_fork(self, s, edits, scope);
        s.assert_revert = saved_assert;

        let after = fingerprints(s, &edited);
        if let Err(paths) = revert_intact(&before, &after) {
            s.poisoned = Some(format!(
                "text of {} file(s) differs after revert",
                paths.len()
            ));
            return Err(CheckError::RevertMismatch { paths });
        }
        outcome.map_err(|e| map_error(e, &edited))
    }
}

/// fork, apply, diagnostics, discard; discard always runs. The `Err` that wins: apply, then
/// diagnostics, then discard (the same as `app::verify`).
fn run_fork(
    a: &RaAnalyzer,
    s: &mut RaSession,
    edits: &[Edit],
    scope: Scope,
) -> Result<ScopedFindings, AnalyzerError> {
    let mut f = a.fork(s)?;
    let applied = a.apply(s, &mut f, edits);
    let checked = match applied {
        Ok(()) => Some(a.diagnostics(s, &f, scope)),
        Err(_) => None,
    };
    let discarded = a.discard(s, f);
    applied?;
    let checked = checked.unwrap_or_else(|| Ok(ScopedFindings::default()))?;
    discarded?;
    Ok(checked)
}
