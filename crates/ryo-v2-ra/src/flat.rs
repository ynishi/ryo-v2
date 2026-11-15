//! The flat reference check (D11, R1 of stage 4): one write of edits on the session's own
//! database with rust-analyzer's setters, a diagnosis, and a write-back of the base texts and
//! source roots, checked by the fingerprints of the texts and, with `assert_revert`, by a
//! re-assert of the write's scope against the baseline. It runs with no fork live, has no stack
//! and runs no line of the layer code (`engine`, `forks`): it is the independent side of every
//! differential test of the Engine, anchored itself by a fresh load (`reference_check` C2).

use crate::check::{finish_check, map_error};
use crate::diagnostics::{Deadline, Detector, collect_errors_par, panic_message};
use crate::fork::{self, Action, Below, LayerState};
use crate::rev_deps::scoped_files;
use crate::session::RaSession;
use crate::stack;
use crate::{DbWritePanic, RaAnalyzer};
use ra_ap_base_db::salsa::Durability;
use ra_ap_base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ra_ap_hir::ChangeWithProcMacros;
use ra_ap_ide::AnalysisHost;
use ra_ap_vfs::{FileId, VfsPath};
use ryo_v2_core::app::ports::{AnalyzerError, CheckError, Event};
use ryo_v2_core::domain::baseline::{restrict, same_findings};
use ryo_v2_core::domain::text::revert_intact;
use ryo_v2_core::domain::{Edit, Finding, Fingerprint, RelPath, Scope, ScopedFindings};
use std::panic::{AssertUnwindSafe, catch_unwind};
use triomphe::Arc as TriArc;

/// What the re-assert after a write-back compares: the scope of the edited paths on the base,
/// and the baseline's findings there.
type Expected = (Vec<(FileId, RelPath)>, Vec<Finding>);

/// What the write of a reference check changed on the base, for its write-back.
#[derive(Default)]
struct Written {
    /// Files of the base the write modified, with their base text.
    modified: Vec<(FileId, RelPath, String)>,
    /// The first save of each source root the write changed.
    saved_roots: Vec<(SourceRootId, TriArc<SourceRoot>)>,
}

/// The flat reference check: `RaAnalyzer::reference_check`.
pub(crate) fn reference_check(
    a: &RaAnalyzer,
    s: &mut RaSession,
    edits: &[Edit],
    scope: Scope,
) -> Result<ScopedFindings, CheckError> {
    stack::require_free(s)?;
    // With the base frozen, `host` holds an empty database: writing it would check nothing.
    if s.frozen.is_some() {
        return Err(CheckError::ForkInUse);
    }
    let edited = fork::edited_paths(edits);
    // It writes the base, so the revert audit re-diagnoses its paths.
    s.edited_since_audit.extend(edited.iter().cloned());
    let session_error = |s: &RaSession, e: AnalyzerError| match &s.poisoned {
        Some(msg) => CheckError::Poisoned(msg.clone()),
        None => map_error(e, &edited),
    };
    let started_ms = a.clock.now_ms();
    let pure = {
        let text = |id: FileId| fork::db_text(&s.host, id);
        let below = Below::new(None, &s.files, &text);
        fork::resolve_pure(&below, edits)
    };
    let actions = fork::complete_on_session(s, pure).map_err(|e| session_error(s, e))?;
    enter_base_texts(s, &actions);
    // What the re-assert compares after the write-back: the scope of the edited paths on the
    // base, and the baseline's findings there.
    let expected = s.assert_revert.then(|| {
        let scope = base_scope(s, &edited);
        let paths: Vec<RelPath> = scope.iter().map(|(_, r)| r.clone()).collect();
        (scope, restrict(&s.baseline, &paths))
    });

    let mut state = LayerState::new(edited.clone(), None);
    let mut written = Written::default();
    let inject = a.db_write_panic == Some(DbWritePanic::Write);
    if let Err(e) = write_actions(s, &mut state, &mut written, actions, inject) {
        // A panic poisoned the session and the database is not touched again; a refusal is
        // undone, and an undo that panics is that panic, not the refusal.
        let undone = if matches!(e, AnalyzerError::Edit { .. }) {
            undo(s, &state, &written)
        } else {
            Ok(())
        };
        if s.poisoned.is_none() {
            s.base_texts.clear();
        }
        return Err(session_error(s, undone.err().unwrap_or(e)));
    }
    fork::finish_level(&s.files, &s.rev_deps, &mut state);
    let write_ms = a.clock.now_ms().saturating_sub(started_ms);
    let outcome = diagnose(a, s, &state, scope, write_ms);
    write_back(a, s, &state, &written, expected)?;
    outcome
}

/// The files of the base that `Scope::Affected` diagnoses for `edited`; none with no path.
fn base_scope(s: &RaSession, edited: &[RelPath]) -> Vec<(FileId, RelPath)> {
    if edited.is_empty() {
        return Vec::new();
    }
    let refs: Vec<&RelPath> = edited.iter().collect();
    scoped_files(&s.files, &s.rev_deps, &refs)
}

/// Diagnoses the written state at `scope` as a check does (its cut-off and detector rules), and
/// records `Event::Checked` with the write's time plus the diagnosis's when it ends.
fn diagnose(
    a: &RaAnalyzer,
    s: &RaSession,
    state: &LayerState,
    scope: Scope,
    write_ms: u64,
) -> Result<ScopedFindings, CheckError> {
    let diag_scope = match scope {
        Scope::Affected => &state.scope,
        Scope::Workspace => &state.live,
    };
    let deadline = a
        .check_timeout_ms
        .map(|ms| Deadline::after(a.clock.clone(), ms));
    let started_ms = a.clock.now_ms();
    let diagnosed = fork::diagnose_files(a, s, &s.host, diag_scope, &state.live, deadline.as_ref());
    if diagnosed.is_ok() {
        a.observer.observe(Event::Checked {
            scope_files: diag_scope.len(),
            total_files: s.files.len(),
            check_ms: write_ms.saturating_add(a.clock.now_ms().saturating_sub(started_ms)),
        });
    }
    finish_check(a, s, deadline.as_ref(), diagnosed)
}

/// Enters the base text of each file `actions` modify or delete that the record does not hold
/// yet, before anything is written: `Verifier::text` reads the record first, so it describes the
/// base during the write and after a write-back that poisoned the session.
fn enter_base_texts(s: &mut RaSession, actions: &[Action]) {
    for action in actions {
        if let Action::Modify { id, below, .. } | Action::Delete { id, below, .. } = action {
            s.base_texts.entry(*id).or_insert_with(|| below.clone());
        }
    }
}

/// Writes the resolved actions in place on the base, recording each step in `state` and
/// `written` as it is made. `inject` makes the database write of the texts panic (the test hook
/// `DbWritePanic::Write`).
fn write_actions(
    s: &mut RaSession,
    state: &mut LayerState,
    written: &mut Written,
    actions: Vec<Action>,
    inject: bool,
) -> Result<(), AnalyzerError> {
    let mut new_texts: Vec<(FileId, Option<String>)> = Vec::new();
    let mut added: Vec<Action> = Vec::new();
    for action in actions {
        match action {
            Action::Modify {
                id,
                rel,
                below,
                new,
            } => {
                written.modified.push((id, rel, below));
                new_texts.push((id, Some(new)));
            }
            Action::Delete { id, rel, below } => {
                state.deleted.push((id, rel, below));
                new_texts.push((id, None));
            }
            Action::Create { .. } | Action::Revive { .. } => added.push(action),
        }
    }
    for action in added {
        match action {
            Action::Create {
                id,
                rel,
                new,
                path,
                sibling,
            } => {
                add_created_file(s, state, written, (id, path), sibling, &rel)?;
                new_texts.push((id, Some(new)));
            }
            Action::Revive { id, rel, new, path } => {
                add_revived_file(s, state, written, (id, path), &rel)?;
                new_texts.push((id, Some(new)));
            }
            Action::Modify { .. } | Action::Delete { .. } => {}
        }
    }
    remove_deleted_files(s, state, written)?;
    if new_texts.is_empty() {
        return Ok(());
    }
    apply_texts(&mut s.host, &mut s.poisoned, &new_texts, inject)
}

/// Takes back what a refused write had done before it was refused: the source roots and the vfs
/// entries of created files. No text was written yet. A panic restoring a root poisons the
/// session and is the result.
fn undo(s: &mut RaSession, state: &LayerState, written: &Written) -> Result<(), AnalyzerError> {
    let restored = restore_roots(s, written);
    drop_created(s, state);
    restored
}

/// The failure of a write-back step that left the session unusable: `Poisoned`, with the poison
/// the step set, or else (a step that failed without setting one) poisoning the session with `e`.
fn failure(s: &mut RaSession, e: AnalyzerError) -> CheckError {
    let msg = s
        .poisoned
        .get_or_insert_with(|| format!("a write-back step failed: {e}"));
    CheckError::Poisoned(msg.clone())
}

/// Writes the base texts and source roots back, then compares the texts of the files the write
/// modified or deleted with their base texts and, with `expected`, re-diagnoses the edited
/// paths' scope and compares it with the baseline there. Every failure poisons the session: a
/// text that differs, or findings that differ, are `RevertMismatch`; a step that cannot be carried
/// out or confirmed is `Poisoned`. `Event::Reverted` records the write-back unless it could not
/// run to its end. The record of base texts is forgotten unless the session is poisoned.
fn write_back(
    a: &RaAnalyzer,
    s: &mut RaSession,
    state: &LayerState,
    written: &Written,
    expected: Option<Expected>,
) -> Result<(), CheckError> {
    let t = a.clock.now_ms();
    let mut texts: Vec<(FileId, Option<String>)> = Vec::new();
    for (id, rel, text) in written.modified.iter().chain(&state.deleted) {
        let mut text = text.clone();
        if a.revert_fault.as_ref() == Some(rel) {
            text.push('\n');
        }
        texts.push((*id, Some(text)));
    }
    // A created file loses its text (restoring the source root drops it from the module tree).
    texts.extend(
        state
            .created
            .iter()
            .map(|(id, _)| (*id, Some(String::new()))),
    );
    texts.extend(state.revived.iter().map(|(id, _)| (*id, None)));
    if !texts.is_empty() {
        let inject = a.db_write_panic == Some(DbWritePanic::Revert);
        apply_texts(&mut s.host, &mut s.poisoned, &texts, inject).map_err(|e| failure(s, e))?;
    }
    if !written.saved_roots.is_empty() {
        restore_roots(s, written).map_err(|e| failure(s, e))?;
    }
    drop_created(s, state);

    // The texts first: a difference there ends the checks.
    let before: Vec<(RelPath, Fingerprint)> = written
        .modified
        .iter()
        .chain(&state.deleted)
        .map(|(_, rel, text)| (rel.clone(), Fingerprint::of(text.as_bytes())))
        .collect();
    let after: Vec<(RelPath, Fingerprint)> = written
        .modified
        .iter()
        .chain(&state.deleted)
        .filter_map(|(id, rel, _)| {
            let text = fork::db_text(&s.host, *id).ok()?;
            Some((rel.clone(), Fingerprint::of(text.as_bytes())))
        })
        .collect();
    let text_mismatch = revert_intact(&before, &after).err();

    let revert_ok = match (&text_mismatch, expected) {
        (None, Some((scope, expected))) => {
            // The texts are back but the write-back cannot be confirmed: the session is not
            // trusted.
            let found = if a.reassert_fault {
                Err(AnalyzerError::Panicked("injected re-assert fault".into()))
            } else {
                findings_of(a, s, &scope)
            };
            let found = found.map_err(|e| {
                s.poisoned = Some(format!(
                    "the re-assert after the reference check could not run: {e}"
                ));
                failure(s, e)
            })?;
            Some(same_findings(&found, &expected))
        }
        _ => None,
    };
    a.observer.observe(Event::Reverted {
        revert_ms: a.clock.now_ms().saturating_sub(t),
        revert_ok,
    });
    if let Some(paths) = text_mismatch {
        s.poisoned = Some(format!(
            "text of {} file(s) differs after the reference check's write-back",
            paths.len()
        ));
        return Err(CheckError::RevertMismatch { paths });
    }
    if revert_ok == Some(false) {
        s.poisoned = Some(
            "diagnostics after the reference check's write-back differ from the baseline \
             (re-assert)"
                .into(),
        );
        return Err(CheckError::RevertMismatch {
            paths: state.edited.clone(),
        });
    }
    s.base_texts.clear();
    Ok(())
}

/// The findings of `files` on the base, as the database gives them now. Never cut off.
fn findings_of(
    a: &RaAnalyzer,
    s: &RaSession,
    files: &[(FileId, RelPath)],
) -> Result<Vec<Finding>, AnalyzerError> {
    Ok(collect_errors_par(
        &s.host,
        files,
        &s.files,
        s.threads,
        s.gap_detectors,
        None,
        Detector {
            fault: a.detector_fault.as_ref(),
            panics: &s.detector_panics,
            probe: a.thread_count.as_deref(),
            times: a.file_times.as_deref(),
        },
    )?
    .findings)
}

/// Runs a database write; a panic poisons the session (the database may be half-updated) and
/// comes back as `Panicked`.
fn guarded<T>(poisoned: &mut Option<String>, f: impl FnOnce() -> T) -> Result<T, AnalyzerError> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|p| {
        let msg = panic_message(p);
        *poisoned = Some(msg.clone());
        AnalyzerError::Panicked(msg)
    })
}

/// Puts a created file (registered in the vfs as `file_id` at `vfs_path`) into the source root
/// of `sibling_id`, a file of the same crate.
fn add_created_file(
    s: &mut RaSession,
    state: &mut LayerState,
    written: &mut Written,
    (file_id, vfs_path): (FileId, VfsPath),
    sibling_id: FileId,
    rel: &RelPath,
) -> Result<(), AnalyzerError> {
    let first = !s.created_in_db.contains(&file_id);

    let root_id = {
        let db = s.host.raw_database();
        db.file_source_root(sibling_id).source_root_id(db)
    };
    insert_into_root(s, written, root_id, file_id, vfs_path)?;
    let db = s.host.raw_database_mut();
    guarded(&mut s.poisoned, || {
        db.set_file_source_root_with_durability(file_id, root_id, Durability::LOW);
        // Both inputs of a created file enter the database's file map together, which keeps
        // them for good: a layer that creates the file later sets both in itself (the Engine
        // cannot create one input of a file and set the other). The text is written below.
        if first {
            db.set_file_text(file_id, "");
        }
    })?;
    s.created_in_db.insert(file_id);
    state.created.push((file_id, rel.clone()));
    Ok(())
}

/// Puts a file known to the database but not a file of the base (`file_id` at `vfs_path`) back
/// into its source root. A write on the base resolves no revive (`fork::resolve_pure` revives a
/// file only a state of a chain deleted), so this is kept for the completeness of the actions.
fn add_revived_file(
    s: &mut RaSession,
    state: &mut LayerState,
    written: &mut Written,
    (file_id, vfs_path): (FileId, VfsPath),
    rel: &RelPath,
) -> Result<(), AnalyzerError> {
    let root_id = {
        let db = s.host.raw_database();
        db.file_source_root(file_id).source_root_id(db)
    };
    insert_into_root(s, written, root_id, file_id, vfs_path)?;
    state.revived.push((file_id, rel.clone()));
    Ok(())
}

/// Copies the source root with the file added (without it `mod x;` does not resolve), saving
/// the root the first time the write changes it.
fn insert_into_root(
    s: &mut RaSession,
    written: &mut Written,
    root_id: SourceRootId,
    file_id: FileId,
    vfs_path: VfsPath,
) -> Result<(), AnalyzerError> {
    let db = s.host.raw_database_mut();
    let old_root = db.source_root(root_id).source_root(db);
    if !written.saved_roots.iter().any(|(id, _)| *id == root_id) {
        written.saved_roots.push((root_id, old_root.clone()));
    }
    let new_root = fork::root_with(&old_root, Some((file_id, vfs_path)), None);
    guarded(&mut s.poisoned, || {
        db.set_source_root_with_durability(root_id, new_root, Durability::LOW);
    })
}

/// Removes deleted files from their source roots. The vfs is not touched: the file still exists
/// on disk, so writing its text back is enough.
fn remove_deleted_files(
    s: &mut RaSession,
    state: &LayerState,
    written: &mut Written,
) -> Result<(), AnalyzerError> {
    let db = s.host.raw_database_mut();
    for (file_id, _, _) in &state.deleted {
        let root_id = db.file_source_root(*file_id).source_root_id(db);
        let old_root = db.source_root(root_id).source_root(db);
        if !written.saved_roots.iter().any(|(id, _)| *id == root_id) {
            written.saved_roots.push((root_id, old_root.clone()));
        }
        let new_root = fork::root_with(&old_root, None, Some(*file_id));
        guarded(&mut s.poisoned, || {
            db.set_source_root_with_durability(root_id, new_root, Durability::LOW);
        })?;
    }
    Ok(())
}

/// Restores the source roots as they were before the write.
fn restore_roots(s: &mut RaSession, written: &Written) -> Result<(), AnalyzerError> {
    let db = s.host.raw_database_mut();
    for (root_id, root) in &written.saved_roots {
        guarded(&mut s.poisoned, || {
            db.set_source_root_with_durability(*root_id, root.clone(), Durability::LOW);
        })?;
    }
    Ok(())
}

/// Drops the files the write created from the vfs (a path created again gets the same FileId
/// back from the vfs).
fn drop_created(s: &mut RaSession, state: &LayerState) {
    for (_, rel) in &state.created {
        fork::vfs_drop(s, rel);
    }
}

/// Applies texts; `None` removes the file (a deleted file, or a revived one's write-back). A
/// panic poisons the session.
fn apply_texts(
    host: &mut AnalysisHost,
    poisoned: &mut Option<String>,
    texts: &[(FileId, Option<String>)],
    inject: bool,
) -> Result<(), AnalyzerError> {
    let mut change = ChangeWithProcMacros::default();
    for (file_id, text) in texts {
        change.change_file(*file_id, text.clone());
    }
    guarded(poisoned, || {
        if inject {
            panic!("injected database write panic");
        }
        host.apply_change(change);
    })
}
