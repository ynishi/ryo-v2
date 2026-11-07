//! Levels: one write of edits on top of the state below it, diagnosed, and dropped. A fork is a
//! stack of levels over the session's base state, each a layer of the fork Engine (`engine`);
//! the functions here are the level operations that the `Analyzer` port, `Stacked` and
//! `Verifier::check` are built on, and the resolution of a write's edits that `Forks` and the
//! flat reference check (`flat`) share.

use crate::RaAnalyzer;
use crate::check::base_text;
use crate::diagnostics::{Deadline, Detector, GapDetector, collect_errors_par, panic_message};
use crate::rev_deps::{RevDeps, scoped_files};
use crate::session::{ForkPort, RaSession};
use crate::stack;
use ra_ap_base_db::{SourceDatabase, SourceRoot};
use ra_ap_ide::AnalysisHost;
use ra_ap_vfs::file_set::FileSet;
use ra_ap_vfs::{AbsPathBuf, FileExcluded, FileId, VfsPath};
use ryo_v2_core::app::ports::{AnalyzerError, Event};
use ryo_v2_core::domain::{Edit, RelPath, Scope, ScopedFindings, UnseenClass};
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

/// What one write made of the state below it, a layer's or the flat reference's: the files it
/// touched and the
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

/// One write of a fork: its state, its layer, and what a diagnosis of it needs.
pub(crate) struct Level {
    /// The state this write left; the next level's state has it as its parent.
    pub(crate) state: Arc<LayerState>,
    /// Duration of this level's write, from the start of `engine::push_level` to the end of the
    /// write, in ms. Each diagnosis of the level reports it plus its own duration, so that the
    /// time between diagnoses is never counted.
    pub(crate) write_ms: u64,
    /// The level's layer.
    pub(crate) layer: crate::engine::LevelLayer,
}

impl Level {
    /// A level of a write: its state and its layer.
    pub(crate) fn layered(
        state: LayerState,
        layer: crate::engine::LevelLayer,
        write_ms: u64,
    ) -> Level {
        Level {
            state: Arc::new(state),
            write_ms,
            layer,
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

/// The text of `id` at the top level of the live fork, the base text with no level: the top
/// layer's text (`engine::top_text`).
pub(crate) fn top_text(session: &RaSession, id: FileId) -> Result<String, AnalyzerError> {
    crate::engine::top_text(session, id)
}

/// The text of `id` in the database of the base: the frozen base's while a fork is live, else
/// what the database holds now (which only the flat reference check writes, and writes back).
pub(crate) fn base_db_text(session: &RaSession, id: FileId) -> Result<String, AnalyzerError> {
    if session.frozen.is_some() {
        crate::engine::base_text(session, id)
    } else {
        db_text(&session.host, id)
    }
}

/// Runs `f` with a host of the state of the top level of the live fork: the session's host with
/// no fork; one over the top level's layer, or a throwaway child of it once it is sealed
/// (`engine::with_top_host`).
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
        id: stack::open_fork(session, ForkPort::Analyzer)?,
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
    crate::engine::push_level(a, session, edits, edited_paths(edits), sampled)
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
    crate::engine::release_levels(a, session).map_err(RevertFailure::into_analyzer)
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
    // A panic of the diagnosis in the top level's open layer poisons that layer.
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

/// Sets the files of a written `state` (`below`, the files of the state it landed on, without the
/// ones it deleted, with the ones it created and revived) and the files its `Scope::Affected`
/// diagnoses. A layer's write and the flat reference's alike.
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
/// read as [`top_text`] reads them.
pub(crate) fn resolve_pure_at_top(session: &RaSession, edits: &[Edit]) -> Pure {
    let text = |id: FileId| top_text(session, id);
    let below = Below::new(top_state(session).map(|s| &**s), &session.files, &text);
    resolve_pure(&below, edits)
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
/// path. While a fork is live the vfs entries are only added (the base is frozen) and dropped at
/// the thaw; for the flat reference check, with no fork live, a refusal here drops the ones it
/// added.
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
/// refused before it wrote anything (the flat reference check's). Nothing is taken back while
/// the base is frozen (the thaw drops them).
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
