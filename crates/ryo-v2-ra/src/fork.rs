//! Levels: one write of edits applied in place on the session on top of the state below it,
//! diagnosed, and reverted. A fork is a stack of levels over the session's base state; the
//! functions here are the level operations that both the `Analyzer` port and `Verifier::check`
//! are built on.

use crate::check::base_text;
use crate::diagnostics::{Deadline, Detector, GapDetector, collect_errors_par, panic_message};
use crate::rev_deps::{RevDeps, scoped_files};
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
use std::sync::Arc;
use triomphe::Arc as TriArc;

/// The ticket of the live `Analyzer` fork of a session. It is an id and nothing else: the
/// session holds the fork's level, and a ticket that is not the live fork's (another session's,
/// or one already discarded) is refused with `ForkInUse`.
#[derive(Debug, Clone)]
pub struct RaFork {
    pub(crate) id: u64,
}

/// What one write made of the state below it, on both backends: the files it touched and the
/// files of the state it leaves. It belongs to the write's level (or layer), not to the session,
/// so that several states over one base can be resolved against, each through its own chain
/// (`parent`), and never through the session's one stack (D11, stage 3b).
///
/// Built while the write runs and shared, read-only, once the write is done.
pub(crate) struct LayerState {
    /// Paths of this write, sorted, unique.
    pub(crate) edited: Vec<RelPath>,
    /// Files this write created: paths that no state of the chain had ever known.
    pub(crate) created: Vec<(FileId, RelPath)>,
    /// Files this write brought back: known to the chain, deleted by a state below.
    pub(crate) revived: Vec<(FileId, RelPath)>,
    /// Files of the state below that this write deleted, with their text there.
    pub(crate) deleted: Vec<(FileId, RelPath, String)>,
    /// The files of the state this write leaves.
    pub(crate) live: Vec<(FileId, RelPath)>,
    /// The files `Scope::Affected` diagnoses in this state; empty when `edited` is.
    pub(crate) scope: Vec<(FileId, RelPath)>,
    /// The state this write landed on; `None` for a write on the base.
    pub(crate) parent: Option<Arc<LayerState>>,
}

impl LayerState {
    /// The state of a write of `edited` on `parent` with nothing written yet.
    pub(crate) fn new(edited: Vec<RelPath>, parent: Option<Arc<LayerState>>) -> LayerState {
        LayerState {
            edited,
            created: Vec::new(),
            revived: Vec::new(),
            deleted: Vec::new(),
            live: Vec::new(),
            scope: Vec::new(),
            parent,
        }
    }

    /// This state and every state below it, this one first.
    fn chain(&self) -> impl Iterator<Item = &LayerState> {
        std::iter::successors(Some(self), |s| s.parent.as_deref())
    }
}

/// The undo log of a write on the undo-log backend: what its revert restores and re-asserts.
/// Empty on the Engine, where a level is a layer and is dropped instead.
#[derive(Default)]
pub(crate) struct UndoLog {
    /// Files of the level below that this level wrote, with their text at the level below.
    modified: Vec<(FileId, RelPath, String)>,
    /// The first save of each source root this level changed.
    saved_roots: Vec<(SourceRootId, TriArc<SourceRoot>)>,
    /// What the re-assert of the revert compares; `Some` iff the write was sampled.
    sampled: Option<Sampled>,
}

/// One write of a fork: its state, the undo log of the change, and what a diagnosis of it needs.
/// On the Engine (`RaConfig.engine`) the level is a layer and its undo log stays empty.
pub(crate) struct Level {
    /// The state this write left; the next level's state has it as its parent.
    pub(crate) state: Arc<LayerState>,
    /// The undo log; empty on the Engine.
    undo: UndoLog,
    /// Duration of this level's write, from the start of [`push_level`] to the end of the write,
    /// in ms. Each diagnosis of the level reports it plus its own duration, so that the time
    /// between diagnoses is never counted.
    pub(crate) write_ms: u64,
    /// The level's layer on the Engine; `None` on the undo log.
    pub(crate) layer: Option<crate::engine::LevelLayer>,
}

impl Level {
    /// A level of a write on the Engine: its state and its layer, with no undo log.
    pub(crate) fn layered(
        state: LayerState,
        layer: crate::engine::LevelLayer,
        write_ms: u64,
    ) -> Level {
        Level {
            state: Arc::new(state),
            undo: UndoLog::default(),
            write_ms,
            layer: Some(layer),
        }
    }
}

/// The state a write resolves against, given without the session: the top state of the chain
/// below it (`None` for the base), the files of the base, and a reader of the texts of that state.
/// Resolution against it ([`resolve_pure`]) needs no vfs and no disk, so any state of a chain
/// can be resolved against, not only the top of the session's stack.
pub(crate) struct Below<'a> {
    top: Option<&'a LayerState>,
    files: &'a [(FileId, RelPath)],
    text: &'a dyn Fn(FileId) -> Result<String, AnalyzerError>,
}

impl<'a> Below<'a> {
    /// The state `top` over the base's `files`, its texts read by `text`.
    pub(crate) fn new(
        top: Option<&'a LayerState>,
        files: &'a [(FileId, RelPath)],
        text: &'a dyn Fn(FileId) -> Result<String, AnalyzerError>,
    ) -> Below<'a> {
        Below { top, files, text }
    }

    /// The files of this state.
    pub(crate) fn live(&self) -> &'a [(FileId, RelPath)] {
        self.top.map_or(self.files, |t| t.live.as_slice())
    }

    /// Every file the base or some state of the chain knows, by path, deleted ones included; a
    /// newer state's entry wins.
    fn known(&self) -> HashMap<&'a RelPath, FileId> {
        let mut known: HashMap<&'a RelPath, FileId> =
            self.files.iter().map(|(id, r)| (r, *id)).collect();
        let chain: Vec<&'a LayerState> = self.top.map(|t| t.chain().collect()).unwrap_or_default();
        for state in chain.into_iter().rev() {
            known.extend(state.created.iter().map(|(id, r)| (r, *id)));
            known.extend(state.revived.iter().map(|(id, r)| (r, *id)));
            known.extend(state.deleted.iter().map(|(id, r, _)| (r, *id)));
        }
        known
    }

    /// The files some state of the chain created: the only files that are not of load time.
    pub(crate) fn created(&self) -> HashSet<FileId> {
        self.top
            .into_iter()
            .flat_map(LayerState::chain)
            .flat_map(|s| s.created.iter().map(|(id, _)| *id))
            .collect()
    }
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

/// What a write is to do to one path, resolved against the state of the level below and
/// completed on the session ([`complete_on_session`]): a created file has its vfs id, path and
/// sibling, a revived one its path.
pub(crate) enum Action {
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
        id: FileId,
        rel: RelPath,
        new: String,
        path: VfsPath,
        /// The session file whose source root the created file joins.
        sibling: FileId,
    },
    Revive {
        id: FileId,
        rel: RelPath,
        new: String,
        path: VfsPath,
    },
}

/// What a write is to do to one path as [`resolve_pure`] decides it against a [`Below`]: a
/// create is only a candidate until the session checks the disk and gives it a vfs id.
pub(crate) enum Step {
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

impl Step {
    fn rel(&self) -> &RelPath {
        match self {
            Step::Modify { rel, .. }
            | Step::Delete { rel, .. }
            | Step::Create { rel, .. }
            | Step::Revive { rel, .. } => rel,
        }
    }
}

/// The pure resolution of a write's edits: one step per edit, in edit order, up to the first
/// edit it refused, and that refusal.
pub(crate) struct Pure {
    steps: Vec<Step>,
    refused: Option<AnalyzerError>,
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
    db_text_in(host.raw_database(), id)
}

/// [`db_text`] on a database handle.
pub(crate) fn db_text_in(
    db: &ra_ap_ide_db::RootDatabase,
    id: FileId,
) -> Result<String, AnalyzerError> {
    catch_unwind(AssertUnwindSafe(|| db.file_text(id).text(db).to_string()))
        .map_err(|p| AnalyzerError::Panicked(panic_message(p)))
}

/// The text of `id` at the top level of the live fork, the base text with no level: what the
/// database holds now on the undo log, the top layer's text on the Engine.
pub(crate) fn top_text(session: &RaSession, id: FileId) -> Result<String, AnalyzerError> {
    if session.frozen.is_some() {
        crate::engine::top_text(session, id)
    } else {
        db_text(&session.host, id)
    }
}

/// The text of `id` in the database of the base: the frozen base's while a fork is live on the
/// Engine, else what the database holds now (which a fork on the undo log has written).
pub(crate) fn base_db_text(session: &RaSession, id: FileId) -> Result<String, AnalyzerError> {
    if session.frozen.is_some() {
        crate::engine::base_text(session, id)
    } else {
        db_text(&session.host, id)
    }
}

/// Runs `f` with a host of the state of the top level of the live fork: the session's host on
/// the undo log and with no fork; on the Engine one over the top level's layer, or a throwaway
/// child of it once it is sealed (`engine::with_top_host`).
pub(crate) fn with_top_host<R>(
    session: &RaSession,
    f: impl FnOnce(&AnalysisHost) -> R,
) -> Result<R, AnalyzerError> {
    crate::engine::with_top_host(session, f)
}

/// The state of the top level of the live fork; `None` with no level.
pub(crate) fn top_state(session: &RaSession) -> Option<&Arc<LayerState>> {
    session.stack.last().map(|l| &l.state)
}

/// The files of the state the next write lands on: the top level's, or the session's own.
pub(crate) fn live_below(session: &RaSession) -> &[(FileId, RelPath)] {
    top_state(session).map_or(session.files.as_slice(), |s| s.live.as_slice())
}

/// The text of `path` at the top level (the base text with no level); `None` when it is not a
/// file of that state.
pub(crate) fn text_at_top(
    session: &RaSession,
    path: &RelPath,
) -> Result<Option<String>, AnalyzerError> {
    match live_below(session).iter().find(|(_, r)| r == path) {
        Some((id, _)) => top_text(session, *id).map(Some),
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
            probe: a.thread_count.as_deref(),
            times: a.file_times.as_deref(),
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
    let engine = session.engine;
    Ok(RaFork {
        id: stack::open_fork(session, ForkPort::Analyzer, engine)?,
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
    let state = &top.state;
    let diag_scope = match scope {
        Scope::Affected => &state.scope,
        Scope::Workspace => &state.live,
    };
    let diagnosed = with_top_host(session, |host| {
        diagnose_files(a, session, host, diag_scope, &state.live, deadline)
    })?;
    // On the Engine a panic of the diagnosis in the top level's open layer poisons that layer.
    if let Err(AnalyzerError::Panicked(msg)) = &diagnosed {
        crate::engine::poison_top_if_open(session, msg);
    }
    let diagnosed = diagnosed?;
    a.observer.observe(Event::Checked {
        scope_files: diag_scope.len(),
        total_files: session.files.len(),
        check_ms: top
            .write_ms
            .saturating_add(a.clock.now_ms().saturating_sub(started_ms)),
    });
    Ok(diagnosed)
}

/// Diagnoses `diag_scope` on `host`, as [`diagnose_level`] does for a level, with `live` the
/// files a diagnostic range may be attributed to; records no event.
pub(crate) fn diagnose_files(
    a: &RaAnalyzer,
    session: &RaSession,
    host: &AnalysisHost,
    diag_scope: &[(FileId, RelPath)],
    live: &[(FileId, RelPath)],
    deadline: Option<&Deadline>,
) -> Result<Diagnosed, AnalyzerError> {
    diagnose_on(
        host,
        diag_scope,
        live,
        session.threads,
        session.gap_detectors,
        deadline,
        Detector {
            fault: a.detector_fault.as_ref(),
            panics: &session.detector_panics,
            probe: a.thread_count.as_deref(),
            times: a.file_times.as_deref(),
        },
        a.diagnose_panic,
    )
}

/// [`diagnose_files`] given what it needs without the session, for a diagnosis on a worker of
/// `Forks`: `threads` and `gap_detectors` as the collect takes them, `detector` where its panics
/// go, and `diagnose_panic` the test hook that makes the diagnosis fail as a panic of
/// rust-analyzer in a query would.
#[allow(clippy::too_many_arguments)]
pub(crate) fn diagnose_on(
    host: &AnalysisHost,
    diag_scope: &[(FileId, RelPath)],
    live: &[(FileId, RelPath)],
    threads: usize,
    gap_detectors: bool,
    deadline: Option<&Deadline>,
    detector: Detector<'_>,
    diagnose_panic: bool,
) -> Result<Diagnosed, AnalyzerError> {
    if diagnose_panic {
        return Err(AnalyzerError::Panicked(
            "injected diagnosis panic".to_owned(),
        ));
    }
    let collected = collect_errors_par(
        host,
        diag_scope,
        live,
        threads,
        gap_detectors,
        deadline,
        detector,
    )?;
    let mut files: Vec<RelPath> = diag_scope.iter().map(|(_, r)| r.clone()).collect();
    files.sort();
    files.dedup();
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
            let edited: BTreeSet<&RelPath> = session
                .stack
                .iter()
                .flat_map(|l| l.state.edited.iter())
                .collect();
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
        .flat_map(|l| l.state.edited.iter())
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

/// The first edit of `state` that a write of it on the base refuses as `resolve_pure` does
/// there: a delete of a path that is not a file of the base. A write of `state` on the base
/// refuses nothing else that a level could have written, so with `state` the state of the top
/// level (`state_is_top`) this is the one refusal left to look for.
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
    if session.frozen.is_some() {
        return crate::engine::push_level(a, session, edits, edited, sampled);
    }
    let write_started_ms = a.clock.now_ms();
    let (pure, below_created) = resolve_pure_at_top(session, edits);
    let actions = complete_on_session(session, pure)?;
    enter_base_texts(session, &below_created, &actions);

    let sampled = if sampled {
        match sample_below(a, session, &edited) {
            Ok(sampled) => Some(sampled),
            Err(e) => {
                // Nothing is written yet: only the vfs entries of the files to create go back.
                forget_created(session, &actions);
                return Err(e);
            }
        }
    } else {
        None
    };

    let mut state = LayerState::new(edited, top_state(session).cloned());
    let mut undo_log = UndoLog {
        sampled,
        ..UndoLog::default()
    };
    let inject = a.db_write_panic == Some(DbWritePanic::Write);
    if let Err(e) = write_actions(session, &mut state, &mut undo_log, actions, inject) {
        // A panic poisoned the session and the database is not touched again; a refusal is
        // undone, and an undo that panics is that panic, not the refusal.
        if matches!(e, AnalyzerError::Edit { .. }) {
            undo(session, &state, &undo_log)?;
        }
        return Err(e);
    }

    finish_level(live_below(session), &session.rev_deps, &mut state);
    session.stack.push(Level {
        state: Arc::new(state),
        undo: undo_log,
        write_ms: a.clock.now_ms().saturating_sub(write_started_ms),
        layer: None,
    });
    Ok(())
}

/// What the re-assert of a sampled write of `edited` compares after its revert: the scope of
/// those paths at the level below, and the findings it has there (the baseline's on the base).
fn sample_below(
    a: &RaAnalyzer,
    session: &RaSession,
    edited: &[RelPath],
) -> Result<Sampled, AnalyzerError> {
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
    Ok(Sampled {
        below_scope,
        expected,
    })
}

/// Sets the files of a written `state` (`below`, the files of the state it landed on, without the
/// ones it deleted, with the ones it created and revived) and the files its `Scope::Affected`
/// diagnoses. Both backends.
pub(crate) fn finish_level(
    below: &[(FileId, RelPath)],
    rev_deps: &RevDeps,
    state: &mut LayerState,
) {
    let deleted: HashSet<FileId> = state.deleted.iter().map(|(id, _, _)| *id).collect();
    let mut live: Vec<(FileId, RelPath)> = below
        .iter()
        .filter(|(id, _)| !deleted.contains(id))
        .cloned()
        .collect();
    live.extend(state.created.iter().cloned());
    live.extend(state.revived.iter().cloned());
    state.scope = if state.edited.is_empty() {
        Vec::new()
    } else {
        let edited_refs: Vec<&RelPath> = state.edited.iter().collect();
        scoped_files(&live, rev_deps, &edited_refs)
    };
    state.live = live;
}

/// [`resolve_pure`] against the top level of the live fork (the base with no level), its texts
/// read as [`top_text`] reads them; with the files the chain below created.
pub(crate) fn resolve_pure_at_top(session: &RaSession, edits: &[Edit]) -> (Pure, HashSet<FileId>) {
    let text = |id: FileId| top_text(session, id);
    let below = Below::new(top_state(session).map(|s| &**s), &session.files, &text);
    (resolve_pure(&below, edits), below.created())
}

/// Resolves each edit against `below`: a modify or delete of a file of that state, a revive of a
/// file the chain knows and that state lacks, or a candidate create of a path nobody knows. It
/// reads only `below`: no vfs, no disk. It stops at the first edit it refuses (a delete of a path
/// that is not a file of that state, or a text that cannot be read).
pub(crate) fn resolve_pure(below: &Below<'_>, edits: &[Edit]) -> Pure {
    let live_by_rel: HashMap<&RelPath, FileId> =
        below.live().iter().map(|(id, r)| (r, *id)).collect();
    // Every file some state of the chain knows, deleted ones included; built at the first
    // written path that is not live.
    let mut known: Option<HashMap<&RelPath, FileId>> = None;
    let mut steps: Vec<Step> = Vec::new();
    for edit in edits {
        let step = match (live_by_rel.get(&edit.path), &edit.new_text) {
            (Some(&id), Some(new)) => (below.text)(id).map(|text| Step::Modify {
                id,
                rel: edit.path.clone(),
                below: text,
                new: new.clone(),
            }),
            (Some(&id), None) => (below.text)(id).map(|text| Step::Delete {
                id,
                rel: edit.path.clone(),
                below: text,
            }),
            (None, Some(new)) => Ok(
                match known.get_or_insert_with(|| below.known()).get(&edit.path) {
                    Some(&id) => Step::Revive {
                        id,
                        rel: edit.path.clone(),
                        new: new.clone(),
                    },
                    None => Step::Create {
                        rel: edit.path.clone(),
                        new: new.clone(),
                    },
                },
            ),
            (None, None) => Err(edit_err(&edit.path, NOT_IN_THE_STATE_BELOW)),
        };
        match step {
            Ok(step) => steps.push(step),
            Err(e) => {
                return Pure {
                    steps,
                    refused: Some(e),
                };
            }
        }
    }
    Pure {
        steps,
        refused: None,
    }
}

/// Completes a pure resolution on the session, in edit order: a candidate create of a path that
/// exists on disk is refused, then the pure refusal, if any, is the result. The last edit of a
/// path wins. Each create then gets its sibling file and its vfs id, and each revive its vfs
/// path. On the Engine the vfs entries are only added while the base is frozen and dropped at
/// the thaw; on the undo log a refusal here drops the ones it added.
pub(crate) fn complete_on_session(
    session: &mut RaSession,
    pure: Pure,
) -> Result<Vec<Action>, AnalyzerError> {
    let mut steps: Vec<Step> = Vec::new();
    let mut index: HashMap<RelPath, usize> = HashMap::new();
    for step in pure.steps {
        if let Step::Create { rel, .. } = &step
            && session.root.join(rel.as_str()).exists()
        {
            return Err(edit_err(
                rel,
                "target exists on disk but is not indexed (non-.rs or excluded?)",
            ));
        }
        match index.get(step.rel()) {
            Some(&i) => steps[i] = step,
            None => {
                index.insert(step.rel().clone(), steps.len());
                steps.push(step);
            }
        }
    }
    if let Some(e) = pure.refused {
        return Err(e);
    }
    let mut actions: Vec<Action> = Vec::with_capacity(steps.len());
    for step in steps {
        let action = match step {
            Step::Modify {
                id,
                rel,
                below,
                new,
            } => Action::Modify {
                id,
                rel,
                below,
                new,
            },
            Step::Delete { id, rel, below } => Action::Delete { id, rel, below },
            Step::Revive { id, rel, new } => Action::Revive {
                id,
                path: session.vfs.file_path(id).clone(),
                rel,
                new,
            },
            Step::Create { rel, new } => {
                let created = sibling_file(session, &rel).and_then(|sibling| {
                    vfs_create(session, &rel, &new).map(|(id, path)| (sibling, id, path))
                });
                match created {
                    Ok((sibling, id, path)) => Action::Create {
                        id,
                        rel,
                        new,
                        path,
                        sibling,
                    },
                    Err(e) => {
                        forget_created(session, &actions);
                        return Err(e);
                    }
                }
            }
        };
        actions.push(action);
    }
    Ok(actions)
}

/// Takes back the vfs entries of the files `actions` were to create, after a write that was
/// refused before it wrote anything. On the Engine nothing is taken back while the base is
/// frozen (the thaw drops them).
fn forget_created(session: &mut RaSession, actions: &[Action]) {
    if session.frozen.is_some() {
        return;
    }
    for action in actions {
        if let Action::Create { rel, .. } = action {
            vfs_drop(session, rel);
        }
    }
}

/// Enters the base text of each load-time file `actions` modify or delete that the record does
/// not hold yet; at its first touch the text below is the base text. Nothing is written yet.
/// A file the chain below `created` is the one file that is not of load time.
fn enter_base_texts(session: &mut RaSession, created: &HashSet<FileId>, actions: &[Action]) {
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

/// Writes the resolved actions in place, recording each step in `state` and `undo_log` as it is
/// made. `inject` makes the database write of the texts panic (the test hook
/// `DbWritePanic::Write`).
fn write_actions(
    session: &mut RaSession,
    state: &mut LayerState,
    undo_log: &mut UndoLog,
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
                undo_log.modified.push((id, rel, below));
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
                add_created_file(session, state, undo_log, (id, path), sibling, &rel)?;
                new_texts.push((id, Some(new)));
            }
            Action::Revive { id, rel, new, path } => {
                add_revived_file(session, state, undo_log, (id, path), &rel)?;
                new_texts.push((id, Some(new)));
            }
            Action::Modify { .. } | Action::Delete { .. } => {}
        }
    }
    remove_deleted_files(session, state, undo_log)?;
    if new_texts.is_empty() {
        return Ok(());
    }
    apply_texts(&mut session.host, &mut session.poisoned, &new_texts, inject)
}

/// Takes back what a refused write had done before it was refused: the source roots and the vfs
/// entries of created files. No text was written yet. A panic restoring a root poisons the
/// session and is the result.
fn undo(
    session: &mut RaSession,
    state: &LayerState,
    undo_log: &UndoLog,
) -> Result<(), AnalyzerError> {
    let restored = restore_roots(session, undo_log);
    drop_created(session, state);
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
    if session.frozen.is_some() {
        return crate::engine::pop_level(a, session);
    }
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
    let state = &level.state;
    let undo_log = &level.undo;
    let mut texts: Vec<(FileId, Option<String>)> = Vec::new();
    for (id, rel, text) in undo_log.modified.iter().chain(&state.deleted) {
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
        apply_texts(&mut session.host, &mut session.poisoned, &texts, inject)
            .map_err(|e| failure(session, e))?;
    }
    if !undo_log.saved_roots.is_empty() {
        restore_roots(session, undo_log).map_err(|e| failure(session, e))?;
    }
    drop_created(session, state);

    // The texts first: a difference there ends the revert's checks.
    let before: Vec<(RelPath, Fingerprint)> = undo_log
        .modified
        .iter()
        .chain(&state.deleted)
        .map(|(_, rel, text)| (rel.clone(), Fingerprint::of(text.as_bytes())))
        .collect();
    let after: Vec<(RelPath, Fingerprint)> = undo_log
        .modified
        .iter()
        .chain(&state.deleted)
        .filter_map(|(id, rel, _)| {
            let text = db_text(&session.host, *id).ok()?;
            Some((rel.clone(), Fingerprint::of(text.as_bytes())))
        })
        .collect();
    let text_mismatch = revert_intact(&before, &after).err();

    let revert_ok = match (&text_mismatch, &undo_log.sampled) {
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
        return Err(RevertFailure::Mismatch(state.edited.clone()));
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
    if session.frozen.is_some() {
        return crate::engine::release_levels(a, session);
    }
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

/// The session file whose source root a file created at `rel` joins: a file of the same crate.
pub(crate) fn sibling_file(session: &RaSession, rel: &RelPath) -> Result<FileId, AnalyzerError> {
    let rel_path = Path::new(rel.as_str());
    let owner_dir = session
        .rev_deps
        .keys()
        .filter(|d| rel_path.starts_with(d))
        .max_by_key(|d| d.as_os_str().len())
        .cloned();
    session
        .files
        .iter()
        .find(|(_, r)| {
            owner_dir
                .as_ref()
                .is_none_or(|d| Path::new(r.as_str()).starts_with(d))
        })
        .map(|(id, _)| *id)
        .ok_or_else(|| edit_err(rel, "no sibling file to determine the source root"))
}

/// Registers a file created at `rel` with `text` in the vfs and returns its FileId and path
/// (the same FileId as the last time the path was created); records the FileId as one the
/// database's file map will hold. A FileId the vfs refuses leaves no vfs entry and is `Edit`.
pub(crate) fn vfs_create(
    session: &mut RaSession,
    rel: &RelPath,
    text: &str,
) -> Result<(FileId, VfsPath), AnalyzerError> {
    let vfs_path = vfs_path_of(session, rel);
    vfs_set(session, vfs_path.clone(), Some(text.as_bytes().to_vec()));
    match session.vfs.file_id(&vfs_path) {
        Some((id, FileExcluded::No)) => {
            if let Some(base) = session.frozen.as_mut() {
                base.vfs_created.insert(rel.clone());
            }
            Ok((id, vfs_path))
        }
        other => {
            // Leave no vfs entry behind.
            vfs_set(session, vfs_path, None);
            let reason = if other.is_some() {
                "vfs excluded the created file"
            } else {
                "vfs did not assign a FileId"
            };
            Err(edit_err(rel, reason))
        }
    }
}

/// The vfs path of the workspace path `rel`.
fn vfs_path_of(session: &RaSession, rel: &RelPath) -> VfsPath {
    VfsPath::from(AbsPathBuf::assert_utf8(session.root.join(rel.as_str())))
}

/// Sets the contents of `path` in the vfs (`None` removes it) and drops the change the vfs queues
/// for it: nothing reads those changes, so without the drain the queue only grows.
fn vfs_set(session: &mut RaSession, path: VfsPath, contents: Option<Vec<u8>>) {
    session.vfs.set_file_contents(path, contents);
    session.vfs.take_changes();
}

/// Removes the file created at `rel` from the vfs.
pub(crate) fn vfs_drop(session: &mut RaSession, rel: &RelPath) {
    let path = vfs_path_of(session, rel);
    vfs_set(session, path, None);
}

/// `old` with `added` in it and `removed` out of it, as a local source root.
pub(crate) fn root_with(
    old: &SourceRoot,
    added: Option<(FileId, VfsPath)>,
    removed: Option<FileId>,
) -> TriArc<SourceRoot> {
    let mut file_set = FileSet::default();
    for f in old.iter() {
        if Some(f) == removed {
            continue;
        }
        if let Some(p) = old.path_for_file(&f) {
            file_set.insert(f, p.clone());
        }
    }
    if let Some((id, path)) = added {
        file_set.insert(id, path);
    }
    TriArc::new(SourceRoot::new_local(file_set))
}

/// Puts a created file (registered in the vfs as `file_id` at `vfs_path`) into the source root
/// of `sibling_id`, a file of the same crate.
fn add_created_file(
    session: &mut RaSession,
    state: &mut LayerState,
    undo_log: &mut UndoLog,
    (file_id, vfs_path): (FileId, VfsPath),
    sibling_id: FileId,
    rel: &RelPath,
) -> Result<(), AnalyzerError> {
    let first = !session.created_in_db.contains(&file_id);

    let root_id = {
        let db = session.host.raw_database();
        db.file_source_root(sibling_id).source_root_id(db)
    };
    insert_into_root(session, undo_log, root_id, file_id, vfs_path)?;
    let db = session.host.raw_database_mut();
    guarded(&mut session.poisoned, || {
        db.set_file_source_root_with_durability(file_id, root_id, Durability::LOW);
        // Both inputs of a created file enter the database's file map together, which keeps
        // them for good: a layer that creates the file later sets both in itself (the Engine
        // cannot create one input of a file and set the other). The text is written below.
        if first {
            db.set_file_text(file_id, "");
        }
    })?;
    session.created_in_db.insert(file_id);
    state.created.push((file_id, rel.clone()));
    Ok(())
}

/// Puts a file a level below deleted (`file_id` at `vfs_path`) back into its source root.
fn add_revived_file(
    session: &mut RaSession,
    state: &mut LayerState,
    undo_log: &mut UndoLog,
    (file_id, vfs_path): (FileId, VfsPath),
    rel: &RelPath,
) -> Result<(), AnalyzerError> {
    let root_id = {
        let db = session.host.raw_database();
        db.file_source_root(file_id).source_root_id(db)
    };
    insert_into_root(session, undo_log, root_id, file_id, vfs_path)?;
    state.revived.push((file_id, rel.clone()));
    Ok(())
}

/// Copies the source root with the file added (without it `mod x;` does not resolve), saving
/// the root the first time this level changes it.
fn insert_into_root(
    session: &mut RaSession,
    undo_log: &mut UndoLog,
    root_id: SourceRootId,
    file_id: FileId,
    vfs_path: VfsPath,
) -> Result<(), AnalyzerError> {
    let db = session.host.raw_database_mut();
    let old_root = db.source_root(root_id).source_root(db);
    if !undo_log.saved_roots.iter().any(|(id, _)| *id == root_id) {
        undo_log.saved_roots.push((root_id, old_root.clone()));
    }
    let new_root = root_with(&old_root, Some((file_id, vfs_path)), None);
    guarded(&mut session.poisoned, || {
        db.set_source_root_with_durability(root_id, new_root, Durability::LOW);
    })
}

/// Removes deleted files from their source roots. The vfs is not touched: the file still exists
/// on disk, so restoring its text is enough to revert.
fn remove_deleted_files(
    session: &mut RaSession,
    state: &LayerState,
    undo_log: &mut UndoLog,
) -> Result<(), AnalyzerError> {
    let db = session.host.raw_database_mut();
    for (file_id, _, _) in &state.deleted {
        let root_id = db.file_source_root(*file_id).source_root_id(db);
        let old_root = db.source_root(root_id).source_root(db);
        if !undo_log.saved_roots.iter().any(|(id, _)| *id == root_id) {
            undo_log.saved_roots.push((root_id, old_root.clone()));
        }
        let new_root = root_with(&old_root, None, Some(*file_id));
        guarded(&mut session.poisoned, || {
            db.set_source_root_with_durability(root_id, new_root, Durability::LOW);
        })?;
    }
    Ok(())
}

/// Restores the source roots as they were before the level.
fn restore_roots(session: &mut RaSession, undo_log: &UndoLog) -> Result<(), AnalyzerError> {
    let db = session.host.raw_database_mut();
    for (root_id, root) in &undo_log.saved_roots {
        guarded(&mut session.poisoned, || {
            db.set_source_root_with_durability(*root_id, root.clone(), Durability::LOW);
        })?;
    }
    Ok(())
}

/// Drops the files a level created from the vfs; undo log only (on the Engine the thaw drops
/// every file created while the base was frozen).
/// (A path created again gets the same FileId back from the vfs.)
fn drop_created(session: &mut RaSession, state: &LayerState) {
    for (_, rel) in &state.created {
        vfs_drop(session, rel);
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
