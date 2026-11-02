//! Levels: one write of edits applied in place on the session on top of the state below it,
//! diagnosed, and reverted. A fork is a stack of levels over the session's base state; the
//! functions here are the level operations that both the `Analyzer` port and `Verifier::check`
//! are built on.

use crate::check::base_text;
use crate::diagnostics::{Deadline, Detector, GapDetector, collect_errors_par, panic_message};
use crate::rev_deps::scoped_files;
use crate::session::{ForkPort, RaSession};
use crate::stack;
use crate::{DbWritePanic, RaAnalyzer};
use ra_ap_base_db::salsa::Durability;
use ra_ap_base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ra_ap_hir::ChangeWithProcMacros;
use ra_ap_ide::AnalysisHost;
use ra_ap_vfs::file_set::FileSet;
use ra_ap_vfs::{AbsPathBuf, FileExcluded, FileId, VfsPath};
use ryo_v2_core::app::ports::{AnalyzerError, Event};
use ryo_v2_core::domain::baseline::{restrict, same_findings};
use ryo_v2_core::domain::text::revert_intact;
use ryo_v2_core::domain::{
    Edit, Finding, Fingerprint, RelPath, Scope, ScopedFindings, UnseenClass,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use triomphe::Arc as TriArc;

/// The ticket of the live `Analyzer` fork of a session. It is an id and nothing else: the
/// session holds the fork's level, and a ticket that is not the live fork's (another session's,
/// or one already discarded) is refused with `ForkInUse`.
#[derive(Debug, Clone)]
pub struct RaFork {
    pub(crate) id: u64,
}

/// One write of a fork: what it changed, the undo log of the change, and what a diagnosis and a
/// revert of it need.
pub(crate) struct Level {
    /// Paths of this write, sorted, unique.
    edited: Vec<RelPath>,
    /// Files of the level below that this level wrote, with their text at the level below.
    modified: Vec<(FileId, RelPath, String)>,
    /// Files this level created: paths that no level had ever known.
    created: Vec<(FileId, RelPath)>,
    /// Files this level brought back: known to the session, deleted by a level below.
    revived: Vec<(FileId, RelPath)>,
    /// Files of the level below that this level deleted, with their text at the level below.
    deleted: Vec<(FileId, RelPath, String)>,
    /// The first save of each source root this level changed.
    saved_roots: Vec<(SourceRootId, TriArc<SourceRoot>)>,
    /// The files of the state of this level.
    live: Vec<(FileId, RelPath)>,
    /// The files `Scope::Affected` diagnoses at this level; empty when `edited` is.
    scope: Vec<(FileId, RelPath)>,
    /// What the re-assert of the revert compares; `Some` iff the write was sampled.
    sampled: Option<Sampled>,
    /// Duration of this level's write, from the start of [`push_level`] to the end of the write,
    /// in ms. Each diagnosis of the level reports it plus its own duration, so that the time
    /// between diagnoses is never counted.
    write_ms: u64,
}

/// The re-assert of a sampled write: the scope it diagnoses after the revert and the findings
/// that scope had before the write.
struct Sampled {
    /// The same edits' scope at the level below; empty when the write has no edit.
    below_scope: Vec<(FileId, RelPath)>,
    /// The findings of `below_scope` before this write.
    expected: Vec<Finding>,
}

/// Why a level could not be reverted. Every failure leaves the session poisoned.
pub(crate) enum RevertFailure {
    /// The revert did not restore the texts of these files, or (when asserted) the findings of
    /// the level's scope.
    Mismatch(Vec<RelPath>),
    /// The revert could not be carried out or confirmed (a database write panicked, or the
    /// re-assert could not run), or the session was already poisoned and nothing was written for
    /// this revert. The text names the cause.
    Poisoned(String),
}

impl RevertFailure {
    /// The failure as the `Analyzer` port says it.
    pub(crate) fn into_analyzer(self) -> AnalyzerError {
        match self {
            RevertFailure::Mismatch(_) => AnalyzerError::RevertMismatch,
            RevertFailure::Poisoned(msg) => AnalyzerError::Poisoned(msg),
        }
    }
}

/// What a write is to do to one path, resolved against the state of the level below.
enum Action {
    Modify {
        id: FileId,
        rel: RelPath,
        below: String,
        new: String,
    },
    Delete {
        id: FileId,
        rel: RelPath,
        below: String,
    },
    Create {
        rel: RelPath,
        new: String,
    },
    Revive {
        id: FileId,
        rel: RelPath,
        new: String,
    },
}

fn edit_err(path: &RelPath, reason: impl Into<String>) -> AnalyzerError {
    AnalyzerError::Edit {
        path: path.clone(),
        reason: reason.into(),
    }
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

/// The text of `id` as the database holds it now. `Files::file_text` panics on an unknown id,
/// so this is wrapped.
pub(crate) fn db_text(host: &AnalysisHost, id: FileId) -> Result<String, AnalyzerError> {
    catch_unwind(AssertUnwindSafe(|| {
        let db = host.raw_database();
        db.file_text(id).text(db).to_string()
    }))
    .map_err(|p| AnalyzerError::Panicked(panic_message(p)))
}

/// The files of the state the next write lands on: the top level's, or the session's own.
fn live_below(session: &RaSession) -> &[(FileId, RelPath)] {
    session
        .stack
        .last()
        .map_or(session.files.as_slice(), |l| l.live.as_slice())
}

/// The text of `path` at the top level (the base text with no level); `None` when it is not a
/// file of that state.
pub(crate) fn text_at_top(
    session: &RaSession,
    path: &RelPath,
) -> Result<Option<String>, AnalyzerError> {
    match live_below(session).iter().find(|(_, r)| r == path) {
        Some((id, _)) => db_text(&session.host, *id).map(Some),
        None => Ok(None),
    }
}

/// The findings of `files`, as the database gives them now, with `known` the files a
/// diagnostic range may be attributed to. Never cut off.
fn findings_of(
    a: &RaAnalyzer,
    session: &RaSession,
    files: &[(FileId, RelPath)],
    known: &[(FileId, RelPath)],
) -> Result<Vec<Finding>, AnalyzerError> {
    Ok(collect_errors_par(
        &session.host,
        files,
        known,
        session.threads,
        session.gap_detectors,
        None,
        Detector {
            fault: a.detector_fault.as_ref(),
            panics: &session.detector_panics,
        },
    )?
    .findings)
}

/// The refusal of an `Analyzer` operation on a poisoned session: the database is not written or
/// read again.
fn refuse_poisoned(session: &RaSession) -> Result<(), AnalyzerError> {
    match &session.poisoned {
        Some(msg) => Err(AnalyzerError::Poisoned(msg.clone())),
        None => Ok(()),
    }
}

pub(crate) fn fork(session: &mut RaSession) -> Result<RaFork, AnalyzerError> {
    refuse_poisoned(session)?;
    if session.live.is_some() {
        return Err(AnalyzerError::ForkInUse);
    }
    Ok(RaFork {
        id: stack::open_fork(session, ForkPort::Analyzer),
    })
}

pub(crate) fn apply(
    a: &RaAnalyzer,
    session: &mut RaSession,
    fork: &RaFork,
    edits: &[Edit],
) -> Result<(), AnalyzerError> {
    if !stack::is_live(session, fork.id, ForkPort::Analyzer) {
        return Err(AnalyzerError::ForkInUse);
    }
    refuse_poisoned(session)?;
    if !session.stack.is_empty() {
        return Err(AnalyzerError::AlreadyApplied);
    }
    let sampled = session.assert_revert;
    push_level(a, session, edits, edited_paths(edits), sampled)
}

pub(crate) fn diagnostics(
    a: &RaAnalyzer,
    session: &RaSession,
    fork: &RaFork,
    scope: Scope,
) -> Result<ScopedFindings, AnalyzerError> {
    if !stack::is_live(session, fork.id, ForkPort::Analyzer) {
        return Err(AnalyzerError::ForkInUse);
    }
    refuse_poisoned(session)?;
    diagnose_level(a, session, scope, None).map(|(found, _)| found)
}

pub(crate) fn discard(
    a: &RaAnalyzer,
    session: &mut RaSession,
    fork: &RaFork,
) -> Result<(), AnalyzerError> {
    if !stack::is_live(session, fork.id, ForkPort::Analyzer) {
        return Err(AnalyzerError::ForkInUse);
    }
    release_levels(a, session).map_err(RevertFailure::into_analyzer)
}

/// The findings and the files of one diagnosis, and the detector and file of each detector
/// panic contained in it.
pub(crate) type Diagnosed = (ScopedFindings, Vec<(GapDetector, RelPath)>);

/// Diagnoses the top level; with no level, nothing. `deadline` cuts the diagnosis off with
/// `Cancelled`. A diagnosis that ends in an error records no event; one that ends records
/// `Event::Checked` with the level's write time plus the time of this diagnosis.
pub(crate) fn diagnose_level(
    a: &RaAnalyzer,
    session: &RaSession,
    scope: Scope,
    deadline: Option<&Deadline>,
) -> Result<Diagnosed, AnalyzerError> {
    let Some(top) = session.stack.last() else {
        return Ok((ScopedFindings::default(), Vec::new()));
    };
    let started_ms = a.clock.now_ms();
    let diag_scope = match scope {
        Scope::Affected => &top.scope,
        Scope::Workspace => &top.live,
    };
    let collected = collect_errors_par(
        &session.host,
        diag_scope,
        &top.live,
        session.threads,
        session.gap_detectors,
        deadline,
        Detector {
            fault: a.detector_fault.as_ref(),
            panics: &session.detector_panics,
        },
    )?;
    let mut files: Vec<RelPath> = diag_scope.iter().map(|(_, r)| r.clone()).collect();
    files.sort();
    files.dedup();
    a.observer.observe(Event::Checked {
        scope_files: diag_scope.len(),
        total_files: session.files.len(),
        check_ms: top
            .write_ms
            .saturating_add(a.clock.now_ms().saturating_sub(started_ms)),
    });
    let mut unseen_at: BTreeMap<UnseenClass, BTreeSet<RelPath>> = BTreeMap::new();
    if !collected.unexpanded_macro.is_empty() {
        unseen_at.insert(
            UnseenClass::UnexpandedMacro,
            collected.unexpanded_macro.into_iter().collect(),
        );
    }
    Ok((
        ScopedFindings {
            files,
            findings: collected.findings,
            unseen_at,
        },
        collected.panics,
    ))
}

/// The files a one-level check at `scope` covers for the edits that take the base to the top
/// level's state (`Stacked::files_of`). Those edits' paths are the paths some level edited whose
/// presence or text at the top level differs from the base: present at the base when a file of
/// the session, at the top when a file of the top level's state. With no such path a check
/// diagnoses no file at `Scope::Affected`; at `Scope::Workspace` it diagnoses every file of the
/// state, the base's with no level. A base text that cannot be read is the error.
pub(crate) fn files_of(
    a: &RaAnalyzer,
    session: &RaSession,
    scope: Scope,
) -> Result<Vec<RelPath>, AnalyzerError> {
    let live = live_below(session);
    let covered: Vec<(FileId, RelPath)> = match scope {
        Scope::Workspace => live.to_vec(),
        Scope::Affected => {
            let edited: BTreeSet<&RelPath> =
                session.stack.iter().flat_map(|l| l.edited.iter()).collect();
            let mut changed: Vec<&RelPath> = Vec::new();
            for path in edited {
                if text_at_top(session, path)? != base_text(a, session, path)? {
                    changed.push(path);
                }
            }
            if changed.is_empty() {
                Vec::new()
            } else {
                scoped_files(live, &session.rev_deps, &changed)
            }
        }
    };
    let mut files: Vec<RelPath> = covered.into_iter().map(|(_, r)| r).collect();
    files.sort();
    files.dedup();
    Ok(files)
}

/// Whether `state` (edits over the base) is the state of the top level: every path some level
/// edited and every path of `state` has, at the top level, the text `state` gives it (the last
/// edit of the path wins; `None` is absent), or its base text when `state` does not name it.
/// Every other path no level edited is at its base text at the top level too. A text that cannot
/// be read, at the top or at the base, is the error.
pub(crate) fn state_is_top(
    a: &RaAnalyzer,
    session: &RaSession,
    state: &[Edit],
) -> Result<bool, AnalyzerError> {
    let mut wanted: BTreeMap<&RelPath, Option<&str>> = BTreeMap::new();
    for edit in state {
        wanted.insert(&edit.path, edit.new_text.as_deref());
    }
    let edited: BTreeSet<&RelPath> = session
        .stack
        .iter()
        .flat_map(|l| l.edited.iter())
        .chain(wanted.keys().copied())
        .collect();
    for path in edited {
        let at_top = text_at_top(session, path)?;
        let same = match wanted.get(path) {
            Some(text) => at_top.as_deref() == *text,
            None => at_top == base_text(a, session, path)?,
        };
        if !same {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Why a write refuses a delete of a path that is not a file of the state it lands on.
pub(crate) const NOT_IN_THE_STATE_BELOW: &str =
    "cannot delete a file that is not in the state below";

/// The first edit of `state` that a write of it on the base refuses as `resolve` does there: a
/// delete of a path that is not a file of the base. A write of `state` on the base refuses
/// nothing else that a level could have written, so with `state` the state of the top level
/// (`state_is_top`) this is the one refusal left to look for.
pub(crate) fn refused_delete_at_base<'s>(
    session: &RaSession,
    state: &'s [Edit],
) -> Option<&'s Edit> {
    state
        .iter()
        .find(|e| e.new_text.is_none() && !session.files.iter().any(|(_, r)| *r == e.path))
}

/// The paths of `edits`, sorted, unique.
pub(crate) fn edited_paths(edits: &[Edit]) -> Vec<RelPath> {
    let mut edited: Vec<RelPath> = edits.iter().map(|e| e.path.clone()).collect();
    edited.sort();
    edited.dedup();
    edited
}

/// Resolves `edits` (whose paths are `edited`, from [`edited_paths`]) against the state of the
/// level below, writes them in place and pushes the level. `sampled` says that the revert of this level re-diagnoses its scope and compares it
/// with what the scope had before the write.
///
/// A refused write (`Edit`, or a failure of the diagnosis that sampling takes first) pushes no
/// level and leaves every text as it was. A panic in a database write poisons the session and
/// is not undone.
pub(crate) fn push_level(
    a: &RaAnalyzer,
    session: &mut RaSession,
    edits: &[Edit],
    edited: Vec<RelPath>,
    sampled: bool,
) -> Result<(), AnalyzerError> {
    let write_started_ms = a.clock.now_ms();
    let actions = resolve(session, edits)?;
    enter_base_texts(session, &actions);

    let sampled = if sampled {
        let below_scope = if edited.is_empty() {
            Vec::new()
        } else {
            let edited_refs: Vec<&RelPath> = edited.iter().collect();
            scoped_files(live_below(session), &session.rev_deps, &edited_refs)
        };
        let expected = if session.stack.is_empty() {
            let paths: Vec<RelPath> = below_scope.iter().map(|(_, r)| r.clone()).collect();
            restrict(&session.baseline, &paths)
        } else {
            findings_of(a, session, &below_scope, live_below(session))?
        };
        Some(Sampled {
            below_scope,
            expected,
        })
    } else {
        None
    };

    let mut level = Level {
        edited,
        modified: Vec::new(),
        created: Vec::new(),
        revived: Vec::new(),
        deleted: Vec::new(),
        saved_roots: Vec::new(),
        live: Vec::new(),
        scope: Vec::new(),
        sampled,
        write_ms: 0,
    };
    let inject = a.db_write_panic == Some(DbWritePanic::Write);
    if let Err(e) = write_actions(session, &mut level, actions, inject) {
        // A panic poisoned the session and the database is not touched again; a refusal is
        // undone, and an undo that panics is that panic, not the refusal.
        if matches!(e, AnalyzerError::Edit { .. }) {
            undo(session, &level)?;
        }
        return Err(e);
    }

    let deleted: HashSet<FileId> = level.deleted.iter().map(|(id, _, _)| *id).collect();
    let mut live: Vec<(FileId, RelPath)> = live_below(session)
        .iter()
        .filter(|(id, _)| !deleted.contains(id))
        .cloned()
        .collect();
    live.extend(level.created.iter().cloned());
    live.extend(level.revived.iter().cloned());
    level.scope = if level.edited.is_empty() {
        Vec::new()
    } else {
        let edited_refs: Vec<&RelPath> = level.edited.iter().collect();
        scoped_files(&live, &session.rev_deps, &edited_refs)
    };
    level.live = live;
    level.write_ms = a.clock.now_ms().saturating_sub(write_started_ms);
    session.stack.push(level);
    Ok(())
}

/// Resolves each edit against the files of the level below. The last edit of a path wins.
fn resolve(session: &RaSession, edits: &[Edit]) -> Result<Vec<Action>, AnalyzerError> {
    let live_by_rel: HashMap<&RelPath, FileId> =
        live_below(session).iter().map(|(id, r)| (r, *id)).collect();
    // Every file some level of this fork knows, deleted ones included; built at the first
    // written path that is not live.
    let mut known: Option<HashMap<&RelPath, FileId>> = None;

    let mut actions: Vec<Action> = Vec::new();
    let mut index: HashMap<&RelPath, usize> = HashMap::new();
    for edit in edits {
        let action = match (live_by_rel.get(&edit.path), &edit.new_text) {
            (Some(&id), Some(new)) => Action::Modify {
                id,
                rel: edit.path.clone(),
                below: db_text(&session.host, id)?,
                new: new.clone(),
            },
            (Some(&id), None) => Action::Delete {
                id,
                rel: edit.path.clone(),
                below: db_text(&session.host, id)?,
            },
            (None, Some(new)) => match known
                .get_or_insert_with(|| known_files(session))
                .get(&edit.path)
            {
                Some(&id) => Action::Revive {
                    id,
                    rel: edit.path.clone(),
                    new: new.clone(),
                },
                None => {
                    if session.root.join(edit.path.as_str()).exists() {
                        return Err(edit_err(
                            &edit.path,
                            "target exists on disk but is not indexed (non-.rs or excluded?)",
                        ));
                    }
                    Action::Create {
                        rel: edit.path.clone(),
                        new: new.clone(),
                    }
                }
            },
            (None, None) => return Err(edit_err(&edit.path, NOT_IN_THE_STATE_BELOW)),
        };
        match index.get(&edit.path) {
            Some(&i) => actions[i] = action,
            None => {
                index.insert(&edit.path, actions.len());
                actions.push(action);
            }
        }
    }
    Ok(actions)
}

/// Every file the session or some level of its fork knows, by path, deleted ones included.
fn known_files(session: &RaSession) -> HashMap<&RelPath, FileId> {
    let mut known: HashMap<&RelPath, FileId> =
        session.files.iter().map(|(id, r)| (r, *id)).collect();
    for level in &session.stack {
        known.extend(level.created.iter().map(|(id, r)| (r, *id)));
        known.extend(level.revived.iter().map(|(id, r)| (r, *id)));
        known.extend(level.deleted.iter().map(|(id, r, _)| (r, *id)));
    }
    known
}

/// Enters the base text of each load-time file `actions` modify or delete that the record does
/// not hold yet; at its first touch the text below is the base text. Nothing is written yet.
/// A file a level created is the one file that is not of load time.
fn enter_base_texts(session: &mut RaSession, actions: &[Action]) {
    let created: HashSet<FileId> = session
        .stack
        .iter()
        .flat_map(|l| l.created.iter().map(|(id, _)| *id))
        .collect();
    for action in actions {
        if let Action::Modify { id, below, .. } | Action::Delete { id, below, .. } = action
            && !created.contains(id)
        {
            session
                .base_texts
                .entry(*id)
                .or_insert_with(|| below.clone());
        }
    }
}

/// Writes the resolved actions in place, recording each step in `level` as it is made.
/// `inject` makes the database write of the texts panic (the test hook `DbWritePanic::Write`).
fn write_actions(
    session: &mut RaSession,
    level: &mut Level,
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
                level.modified.push((id, rel, below));
                new_texts.push((id, Some(new)));
            }
            Action::Delete { id, rel, below } => {
                level.deleted.push((id, rel, below));
                new_texts.push((id, None));
            }
            Action::Create { .. } | Action::Revive { .. } => added.push(action),
        }
    }
    for action in added {
        match action {
            Action::Create { rel, new } => {
                let id = add_created_file(session, level, &rel, &new)?;
                new_texts.push((id, Some(new)));
            }
            Action::Revive { id, rel, new } => {
                add_revived_file(session, level, id, &rel)?;
                new_texts.push((id, Some(new)));
            }
            Action::Modify { .. } | Action::Delete { .. } => {}
        }
    }
    remove_deleted_files(session, level)?;
    if new_texts.is_empty() {
        return Ok(());
    }
    apply_texts(&mut session.host, &mut session.poisoned, &new_texts, inject)
}

/// Takes back what a refused write had done before it was refused: the source roots and the vfs
/// entries of created files. No text was written yet. A panic restoring a root poisons the
/// session and is the result.
fn undo(session: &mut RaSession, level: &Level) -> Result<(), AnalyzerError> {
    let restored = restore_roots(session, level);
    drop_created(session, level);
    restored
}

/// Reverts the top level and returns `true`; with no level, nothing happens and it is `false`.
///
/// The level is removed whatever happens. The texts of the files it modified or deleted are
/// compared with their texts before the write on every revert, and the findings of its scope
/// when the write was sampled; a difference poisons the session. Every failure of a revert
/// poisons the session: one that cannot be carried out or confirmed leaves a session that cannot
/// be trusted. An `Event::Reverted` records the revert unless it could not run to its end.
pub(crate) fn pop_level(a: &RaAnalyzer, session: &mut RaSession) -> Result<bool, RevertFailure> {
    let Some(level) = session.stack.pop() else {
        return Ok(false);
    };
    let result = revert_level(a, session, &level);
    if session.stack.is_empty() && session.poisoned.is_none() {
        session.base_texts.clear();
    }
    result.map(|()| true)
}

/// The failure of a revert step that left the session unusable: `Poisoned`, with the poison the
/// step set, or else (a step that failed without setting one) poisoning the session with `e`.
fn failure(session: &mut RaSession, e: AnalyzerError) -> RevertFailure {
    let msg = session
        .poisoned
        .get_or_insert_with(|| format!("a revert step failed: {e}"));
    RevertFailure::Poisoned(msg.clone())
}

fn revert_level(
    a: &RaAnalyzer,
    session: &mut RaSession,
    level: &Level,
) -> Result<(), RevertFailure> {
    let t = a.clock.now_ms();
    let mut texts: Vec<(FileId, Option<String>)> = Vec::new();
    for (id, rel, text) in level.modified.iter().chain(&level.deleted) {
        let mut text = text.clone();
        if a.revert_fault.as_ref() == Some(rel) {
            text.push('\n');
        }
        texts.push((*id, Some(text)));
    }
    // A created file loses its text (restoring the source root drops it from the module tree).
    texts.extend(
        level
            .created
            .iter()
            .map(|(id, _)| (*id, Some(String::new()))),
    );
    texts.extend(level.revived.iter().map(|(id, _)| (*id, None)));
    if !texts.is_empty() {
        let inject = a.db_write_panic == Some(DbWritePanic::Revert);
        apply_texts(&mut session.host, &mut session.poisoned, &texts, inject)
            .map_err(|e| failure(session, e))?;
    }
    if !level.saved_roots.is_empty() {
        restore_roots(session, level).map_err(|e| failure(session, e))?;
    }
    drop_created(session, level);

    // The texts first: a difference there ends the revert's checks.
    let before: Vec<(RelPath, Fingerprint)> = level
        .modified
        .iter()
        .chain(&level.deleted)
        .map(|(_, rel, text)| (rel.clone(), Fingerprint::of(text.as_bytes())))
        .collect();
    let after: Vec<(RelPath, Fingerprint)> = level
        .modified
        .iter()
        .chain(&level.deleted)
        .filter_map(|(id, rel, _)| {
            let text = db_text(&session.host, *id).ok()?;
            Some((rel.clone(), Fingerprint::of(text.as_bytes())))
        })
        .collect();
    let text_mismatch = revert_intact(&before, &after).err();

    let revert_ok = match (&text_mismatch, &level.sampled) {
        (None, Some(sampled)) => {
            // The texts are back but the revert cannot be confirmed: the session is not trusted.
            let found = if a.reassert_fault {
                Err(AnalyzerError::Panicked("injected re-assert fault".into()))
            } else {
                findings_of(a, session, &sampled.below_scope, live_below(session))
            };
            let found = found.map_err(|e| {
                session.poisoned = Some(format!("the re-assert after a revert could not run: {e}"));
                failure(session, e)
            })?;
            Some(same_findings(&found, &sampled.expected))
        }
        _ => None,
    };
    a.observer.observe(Event::Reverted {
        revert_ms: a.clock.now_ms().saturating_sub(t),
        revert_ok,
    });
    if let Some(paths) = text_mismatch {
        session.poisoned = Some(format!(
            "text of {} file(s) differs after revert",
            paths.len()
        ));
        return Err(RevertFailure::Mismatch(paths));
    }
    if revert_ok == Some(false) {
        session.poisoned =
            Some("diagnostics after revert differ from those before the write (re-assert)".into());
        return Err(RevertFailure::Mismatch(level.edited.clone()));
    }
    Ok(())
}

/// Ends the fork: reverts every level, top first, under the rules of [`pop_level`].
///
/// A session that is already poisoned is not written again: the levels are dropped and the
/// result is `Poisoned`. A revert that fails has poisoned the session, so the first failure stops
/// the release at once: nothing more is written, the remaining levels are dropped and that
/// failure is the result. The base texts the levels recorded stay with a poisoned session; a
/// session that is not poisoned forgets them.
pub(crate) fn release_levels(a: &RaAnalyzer, session: &mut RaSession) -> Result<(), RevertFailure> {
    session.live = None;
    if let Some(msg) = &session.poisoned {
        let msg = msg.clone();
        session.stack.clear();
        return Err(RevertFailure::Poisoned(msg));
    }
    while !session.stack.is_empty() {
        if let Err(failed) = pop_level(a, session) {
            session.stack.clear();
            return Err(failed);
        }
    }
    // A refused write entered base texts and pushed no level.
    session.base_texts.clear();
    Ok(())
}

/// Registers a created file in the vfs and its sibling's source root; returns its FileId.
fn add_created_file(
    session: &mut RaSession,
    level: &mut Level,
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
    let file_id = match session.vfs.file_id(&vfs_path) {
        Some((id, FileExcluded::No)) => id,
        other => {
            // Leave no vfs entry behind.
            session.vfs.set_file_contents(vfs_path, None);
            let reason = if other.is_some() {
                "vfs excluded the created file"
            } else {
                "vfs did not assign a FileId"
            };
            return Err(edit_err(rel, reason));
        }
    };

    let root_id = {
        let db = session.host.raw_database();
        db.file_source_root(sibling_id).source_root_id(db)
    };
    insert_into_root(session, level, root_id, file_id, vfs_path)?;
    let db = session.host.raw_database_mut();
    guarded(&mut session.poisoned, || {
        db.set_file_source_root_with_durability(file_id, root_id, Durability::LOW);
    })?;
    level.created.push((file_id, rel.clone()));
    Ok(file_id)
}

/// Puts a file a level below deleted back into its source root.
fn add_revived_file(
    session: &mut RaSession,
    level: &mut Level,
    file_id: FileId,
    rel: &RelPath,
) -> Result<(), AnalyzerError> {
    let vfs_path = session.vfs.file_path(file_id).clone();
    let root_id = {
        let db = session.host.raw_database();
        db.file_source_root(file_id).source_root_id(db)
    };
    insert_into_root(session, level, root_id, file_id, vfs_path)?;
    level.revived.push((file_id, rel.clone()));
    Ok(())
}

/// Copies the source root with the file added (without it `mod x;` does not resolve), saving
/// the root the first time this level changes it.
fn insert_into_root(
    session: &mut RaSession,
    level: &mut Level,
    root_id: SourceRootId,
    file_id: FileId,
    vfs_path: VfsPath,
) -> Result<(), AnalyzerError> {
    let db = session.host.raw_database_mut();
    let old_root = db.source_root(root_id).source_root(db);
    if !level.saved_roots.iter().any(|(id, _)| *id == root_id) {
        level.saved_roots.push((root_id, old_root.clone()));
    }
    let mut file_set = FileSet::default();
    for f in old_root.iter() {
        if let Some(p) = old_root.path_for_file(&f) {
            file_set.insert(f, p.clone());
        }
    }
    file_set.insert(file_id, vfs_path);
    guarded(&mut session.poisoned, || {
        db.set_source_root_with_durability(
            root_id,
            TriArc::new(SourceRoot::new_local(file_set)),
            Durability::LOW,
        );
    })
}

/// Removes deleted files from their source roots. The vfs is not touched: the file still exists
/// on disk, so restoring its text is enough to revert.
fn remove_deleted_files(session: &mut RaSession, level: &mut Level) -> Result<(), AnalyzerError> {
    let db = session.host.raw_database_mut();
    for (file_id, _, _) in &level.deleted {
        let root_id = db.file_source_root(*file_id).source_root_id(db);
        let old_root = db.source_root(root_id).source_root(db);
        if !level.saved_roots.iter().any(|(id, _)| *id == root_id) {
            level.saved_roots.push((root_id, old_root.clone()));
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
        guarded(&mut session.poisoned, || {
            db.set_source_root_with_durability(
                root_id,
                TriArc::new(SourceRoot::new_local(file_set)),
                Durability::LOW,
            );
        })?;
    }
    Ok(())
}

/// Restores the source roots as they were before the level.
fn restore_roots(session: &mut RaSession, level: &Level) -> Result<(), AnalyzerError> {
    let db = session.host.raw_database_mut();
    for (root_id, root) in &level.saved_roots {
        guarded(&mut session.poisoned, || {
            db.set_source_root_with_durability(*root_id, root.clone(), Durability::LOW);
        })?;
    }
    Ok(())
}

/// Drops the files a level created from the vfs.
/// (A path created again gets the same FileId back from the vfs.)
fn drop_created(session: &mut RaSession, level: &Level) {
    for (_, rel) in &level.created {
        let abs = AbsPathBuf::assert_utf8(session.root.join(rel.as_str()));
        session.vfs.set_file_contents(VfsPath::from(abs), None);
    }
}

/// Applies texts; `None` removes the file (Deleted / Created revert). A panic poisons the session.
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
