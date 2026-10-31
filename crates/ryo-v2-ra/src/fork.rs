//! Forks: apply edits in place on the session, diagnose, revert.

use crate::RaAnalyzer;
use crate::diagnostics::{
    RawError, collect_errors_par, filter_baseline, sorted_keys, to_diagnostics,
};
use crate::rev_deps::scoped_files;
use crate::session::RaSession;
use ra_ap_base_db::salsa::Durability;
use ra_ap_base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ra_ap_hir::ChangeWithProcMacros;
use ra_ap_ide::AnalysisHost;
use ra_ap_vfs::file_set::FileSet;
use ra_ap_vfs::{AbsPathBuf, FileExcluded, FileId, VfsPath};
use ryo_v2_core::app::ports::{AnalyzerError, Event};
use ryo_v2_core::domain::{Diagnostics, Edit, RelPath, Scope};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use triomphe::Arc as TriArc;

/// A token for one set of in-place edits on a session; it holds what is needed to revert them.
pub struct RaFork {
    applied: bool,
    apply_started_ms: u64,
    /// (file, path, text on disk)
    modified: Vec<(FileId, RelPath, String)>,
    created: Vec<(FileId, RelPath)>,
    deleted: Vec<(FileId, RelPath, String)>,
    saved_roots: Vec<(SourceRootId, TriArc<SourceRoot>)>,
    full_scope: Vec<(FileId, RelPath)>,
}

fn edit_err(path: &RelPath, reason: impl Into<String>) -> AnalyzerError {
    AnalyzerError::Edit {
        path: path.clone(),
        reason: reason.into(),
    }
}

pub(crate) fn fork(session: &mut RaSession) -> Result<RaFork, AnalyzerError> {
    if session.live_fork {
        return Err(AnalyzerError::ForkInUse);
    }
    session.live_fork = true;
    Ok(RaFork {
        applied: false,
        apply_started_ms: 0,
        modified: Vec::new(),
        created: Vec::new(),
        deleted: Vec::new(),
        saved_roots: Vec::new(),
        full_scope: Vec::new(),
    })
}

pub(crate) fn apply(
    a: &RaAnalyzer,
    session: &mut RaSession,
    fork: &mut RaFork,
    edits: &[Edit],
) -> Result<(), AnalyzerError> {
    if fork.applied {
        return Err(AnalyzerError::AlreadyApplied);
    }
    fork.apply_started_ms = a.clock.now_ms();

    // Resolve every path first, so a rejected edit leaves the session untouched.
    // A path not in the workspace is Created; `new_text == None` is Deleted.
    let by_rel: HashMap<&RelPath, FileId> = session.files.iter().map(|(id, r)| (r, *id)).collect();
    let mut modified: Vec<(FileId, RelPath, String, String)> = Vec::new(); // id, path, base, new
    let mut created: Vec<(RelPath, String)> = Vec::new();
    let mut deleted: Vec<(FileId, RelPath, String)> = Vec::new();
    let read_base = |rel: &RelPath| {
        std::fs::read_to_string(session.root.join(rel.as_str()))
            .map_err(|e| edit_err(rel, format!("read base text: {e}")))
    };
    for edit in edits {
        match (by_rel.get(&edit.path), &edit.new_text) {
            (Some(&id), Some(new)) => {
                modified.push((id, edit.path.clone(), read_base(&edit.path)?, new.clone()));
            }
            (Some(&id), None) => deleted.push((id, edit.path.clone(), read_base(&edit.path)?)),
            (None, Some(new)) => {
                if session.root.join(edit.path.as_str()).exists() {
                    return Err(edit_err(
                        &edit.path,
                        "target exists on disk but is not indexed (non-.rs or excluded?)",
                    ));
                }
                created.push((edit.path.clone(), new.clone()));
            }
            (None, None) => {
                return Err(edit_err(
                    &edit.path,
                    "cannot delete a file that is not in the workspace",
                ));
            }
        }
    }

    fork.modified = modified
        .iter()
        .map(|(id, rel, base, _)| (*id, rel.clone(), base.clone()))
        .collect();
    fork.deleted = deleted;

    // Created registration + Deleted removal from the source root.
    let mut created_texts: Vec<(FileId, String)> = Vec::new();
    for (rel, text) in &created {
        let id = add_created_file(session, fork, rel, text)?;
        created_texts.push((id, text.clone()));
    }
    remove_deleted_files(session, fork);

    // Scope: edited crates + dependents. Deleted files cannot be diagnosed (off the root).
    let mut edited: Vec<&RelPath> = fork.modified.iter().map(|(_, r, _)| r).collect();
    edited.extend(fork.created.iter().map(|(_, r)| r));
    edited.extend(fork.deleted.iter().map(|(_, r, _)| r));
    fork.full_scope = scoped_files(&session.files, &session.rev_deps, &edited);

    let mut new_texts: Vec<(FileId, Option<String>)> = modified
        .into_iter()
        .map(|(id, _, _, new)| (id, Some(new)))
        .collect();
    new_texts.extend(created_texts.into_iter().map(|(id, t)| (id, Some(t))));
    new_texts.extend(fork.deleted.iter().map(|(id, _, _)| (*id, None)));
    apply_texts(&mut session.host, &new_texts);
    fork.applied = true;
    Ok(())
}

pub(crate) fn diagnostics(
    a: &RaAnalyzer,
    session: &RaSession,
    fork: &RaFork,
    scope: Scope,
) -> Result<Diagnostics, AnalyzerError> {
    if !fork.applied {
        return Ok(Diagnostics::default());
    }
    let base_scope = match scope {
        Scope::Affected => &fork.full_scope,
        Scope::Workspace => &session.files,
    };
    let mut diag_scope: Vec<(FileId, RelPath)> = base_scope
        .iter()
        .filter(|(id, _)| !fork.deleted.iter().any(|(did, _, _)| did == id))
        .cloned()
        .collect();
    diag_scope.extend(fork.created.iter().cloned());
    let scoped_baseline = filter_baseline(&session.baseline, &diag_scope);
    let known: HashSet<_> = scoped_baseline.iter().map(RawError::key).collect();

    let errors = collect_errors_par(&session.host, &diag_scope, session.threads)?;
    let new_errors: Vec<RawError> = errors
        .into_iter()
        .filter(|e| !known.contains(&e.key()))
        .collect();
    a.observer.observe(Event::Checked {
        scope_files: diag_scope.len(),
        total_files: session.files.len(),
        check_ms: a.clock.now_ms().saturating_sub(fork.apply_started_ms),
    });
    Ok(to_diagnostics(&new_errors))
}

pub(crate) fn discard(
    a: &RaAnalyzer,
    session: &mut RaSession,
    fork: RaFork,
) -> Result<(), AnalyzerError> {
    let result = revert(a, session, &fork);
    session.live_fork = false;
    result
}

fn revert(a: &RaAnalyzer, session: &mut RaSession, fork: &RaFork) -> Result<(), AnalyzerError> {
    let t = a.clock.now_ms();
    let mut base_texts: Vec<(FileId, Option<String>)> = fork
        .modified
        .iter()
        .map(|(id, _, base)| (*id, Some(base.clone())))
        .collect();
    // A created file loses its text (restoring the source root drops it from the module tree).
    base_texts.extend(
        fork.created
            .iter()
            .map(|(id, _)| (*id, Some(String::new()))),
    );
    // A deleted file gets its base text back.
    base_texts.extend(
        fork.deleted
            .iter()
            .map(|(id, _, base)| (*id, Some(base.clone()))),
    );
    if !base_texts.is_empty() {
        apply_texts(&mut session.host, &base_texts);
    }
    if !fork.saved_roots.is_empty() {
        restore_roots(session, fork);
    }
    let revert_ok = if session.assert_revert && fork.applied {
        // Created files are off the root after the revert; deleted ones are back.
        let assert_scope: Vec<(FileId, RelPath)> = fork
            .full_scope
            .iter()
            .filter(|(id, _)| !fork.created.iter().any(|(cid, _)| cid == id))
            .cloned()
            .collect();
        let want = sorted_keys(&filter_baseline(&session.baseline, &assert_scope));
        let after = collect_errors_par(&session.host, &assert_scope, session.threads)?;
        Some(sorted_keys(&after) == want)
    } else {
        None
    };
    a.observer.observe(Event::Reverted {
        revert_ms: a.clock.now_ms().saturating_sub(t),
        revert_ok,
    });
    if revert_ok == Some(false) {
        return Err(AnalyzerError::RevertMismatch);
    }
    Ok(())
}

/// Registers a created file in the vfs and its sibling's source root; returns its FileId.
fn add_created_file(
    session: &mut RaSession,
    fork: &mut RaFork,
    rel: &RelPath,
    text: &str,
) -> Result<FileId, AnalyzerError> {
    // The source root comes from a sibling file of the same crate.
    let rel_path = Path::new(rel.as_str());
    let owner_dir = session
        .rev_deps
        .keys()
        .filter(|d| rel_path.starts_with(d))
        .max_by_key(|d| d.as_os_str().len())
        .cloned();
    let sibling_id = session
        .files
        .iter()
        .find(|(_, r)| {
            owner_dir
                .as_ref()
                .is_none_or(|d| Path::new(r.as_str()).starts_with(d))
        })
        .map(|(id, _)| *id)
        .ok_or_else(|| edit_err(rel, "no sibling file to determine the source root"))?;

    let abs = AbsPathBuf::assert_utf8(session.root.join(rel.as_str()));
    let vfs_path = VfsPath::from(abs);
    session
        .vfs
        .set_file_contents(vfs_path.clone(), Some(text.as_bytes().to_vec()));
    let (file_id, excluded) = session
        .vfs
        .file_id(&vfs_path)
        .ok_or_else(|| edit_err(rel, "vfs did not assign a FileId"))?;
    if matches!(excluded, FileExcluded::Yes) {
        return Err(edit_err(rel, "vfs excluded the created file"));
    }

    // Copy the source root with the new file added (without it `mod x;` does not resolve).
    let db = session.host.raw_database_mut();
    let root_id = db.file_source_root(sibling_id).source_root_id(db);
    let old_root = db.source_root(root_id).source_root(db);
    if !fork.saved_roots.iter().any(|(id, _)| *id == root_id) {
        fork.saved_roots.push((root_id, old_root.clone()));
    }
    let mut file_set = FileSet::default();
    for f in old_root.iter() {
        if let Some(p) = old_root.path_for_file(&f) {
            file_set.insert(f, p.clone());
        }
    }
    file_set.insert(file_id, vfs_path);
    db.set_source_root_with_durability(
        root_id,
        TriArc::new(SourceRoot::new_local(file_set)),
        Durability::LOW,
    );
    db.set_file_source_root_with_durability(file_id, root_id, Durability::LOW);
    fork.created.push((file_id, rel.clone()));
    Ok(file_id)
}

/// Removes deleted files from their source roots. The vfs is not touched: the file still exists
/// on disk, so restoring its text is enough to revert.
fn remove_deleted_files(session: &mut RaSession, fork: &mut RaFork) {
    let db = session.host.raw_database_mut();
    for (file_id, _, _) in &fork.deleted {
        let root_id = db.file_source_root(*file_id).source_root_id(db);
        let old_root = db.source_root(root_id).source_root(db);
        if !fork.saved_roots.iter().any(|(id, _)| *id == root_id) {
            fork.saved_roots.push((root_id, old_root.clone()));
        }
        let mut file_set = FileSet::default();
        for f in old_root.iter() {
            if f == *file_id {
                continue;
            }
            if let Some(p) = old_root.path_for_file(&f) {
                file_set.insert(f, p.clone());
            }
        }
        db.set_source_root_with_durability(
            root_id,
            TriArc::new(SourceRoot::new_local(file_set)),
            Durability::LOW,
        );
    }
}

/// Reverts created files: restores the original source roots and drops them from the vfs.
/// (A path created again gets the same FileId back from the vfs.)
fn restore_roots(session: &mut RaSession, fork: &RaFork) {
    let db = session.host.raw_database_mut();
    for (root_id, root) in &fork.saved_roots {
        db.set_source_root_with_durability(*root_id, root.clone(), Durability::LOW);
    }
    for (_, rel) in &fork.created {
        let abs = AbsPathBuf::assert_utf8(session.root.join(rel.as_str()));
        session.vfs.set_file_contents(VfsPath::from(abs), None);
    }
}

/// Applies texts; `None` removes the file (Deleted / Created revert).
fn apply_texts(host: &mut AnalysisHost, texts: &[(FileId, Option<String>)]) {
    let mut change = ChangeWithProcMacros::default();
    for (file_id, text) in texts {
        change.change_file(*file_id, text.clone());
    }
    host.apply_change(change);
}
