//! The forks of a session on the fork Engine (D11, stage 3; since stage 4 every fork: the flat
//! reference check, `flat`, is the one write of the base outside it).
//!
//! While a fork is live the session's database is the frozen base of the Engine and `host`
//! holds an empty one. A level is a layer: its write goes into a new layer on the level below
//! (or on the base), which seals the level below; its diagnosis runs in the layer while the
//! layer is open, and in a throwaway child of it once it is sealed (E9). `pop` drops the top
//! layer and `release` drops them all and thaws the base, so nothing writes the base: the
//! check that a level's write left the base as it was is the isolation of the Engine (I2), not
//! a revert. Since the `Stacked` port was deleted (stage 4) the forks here, a check's and an
//! `Analyzer` fork, write one level each; writes stack as jobs of `Forks` (`forks`).
//!
//! A panic inside a layer (its write, or a query of its diagnosis) poisons that layer and every
//! layer above it, never the base, a level below or the session: a write that panics pushes no
//! level, and a level whose diagnosis panicked refuses every further diagnosis and every write
//! on it until it is popped. The session is poisoned only when the base was written (a write the
//! frozen base refused, or a fingerprint of the base that changed) or the isolation check could
//! not run.
//!
//! The vfs only gains entries while the base is frozen: a file a layer creates keeps its vfs
//! entry until the thaw, whatever happens to the layer, so that no layer's end takes away an
//! entry another state over the same base was given (a path created again gets the same id).

use crate::diagnostics::panic_message;
use crate::fork::{self, Action, LayerState, Level, RevertFailure};
use crate::session::RaSession;
use crate::{DbWritePanic, RaAnalyzer};
use ra_ap_base_db::{SourceRoot, SourceRootId};
use ra_ap_ide::AnalysisHost;
use ra_ap_ide_db::RootDatabase;
use ra_ap_vfs::FileId;
use ryo_v2_core::app::ports::{AnalyzerError, Event};
use ryo_v2_core::domain::{Edit, RelPath};
use ryo_v2_engine::{Engine, EngineError, Frozen, Isolation, LayerStats, OpenLayer, SealedLayer};
use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Mutex, PoisonError};
use triomphe::Arc as TriArc;

/// The frozen base of a live fork and what its isolation check compares with.
pub(crate) struct EngineBase {
    pub(crate) frozen: Frozen<RootDatabase>,
    /// The base's refused writes at the freeze (the count is kept for the database's life).
    pub(crate) refused_at_freeze: usize,
    /// The isolation of the base when the fork's first sampled write was made: every later
    /// sampled check compares the base with it.
    pub(crate) sampled_from: Option<Isolation>,
    /// Every path given a vfs entry as a created file since the freeze; the thaw drops them.
    pub(crate) vfs_created: BTreeSet<RelPath>,
}

/// The layer of a level, open until a write pushes the next level on it.
pub(crate) struct LevelLayer {
    handle: Handle,
    /// The panic that poisoned the layer, if one did.
    poisoned: Mutex<Option<String>>,
    /// Whether the write of the level was sampled: its `pop` checks the isolation of the base.
    sampled: bool,
}

/// The Engine handle of a level's layer.
enum Handle {
    Open(OpenLayer<RootDatabase>),
    Sealed(SealedLayer<RootDatabase>),
    /// Only inside [`LevelLayer::seal`], which always puts the layer back.
    Taken,
}

/// A level whose layer was lost, or a fork without its frozen base: a fault of the adapter, which
/// no port operation leads to.
fn lost(what: &str) -> AnalyzerError {
    AnalyzerError::Panicked(format!("fork Engine: {what}"))
}

impl LevelLayer {
    fn poison(&self) -> Option<String> {
        self.poisoned
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_poison(&self, msg: &str) {
        let mut poisoned = self.poisoned.lock().unwrap_or_else(PoisonError::into_inner);
        if poisoned.is_none() {
            *poisoned = Some(msg.to_owned());
        }
    }

    /// Seals the layer (once) and returns it as a parent for the next level.
    fn seal(&mut self) -> Result<SealedLayer<RootDatabase>, AnalyzerError> {
        let (handle, sealed) = match std::mem::replace(&mut self.handle, Handle::Taken) {
            Handle::Sealed(sealed) => (Handle::Sealed(sealed.clone()), Ok(sealed)),
            Handle::Open(open) => match open.seal() {
                Ok(sealed) => (Handle::Sealed(sealed.clone()), Ok(sealed)),
                // A clone of the layer's database outlived a diagnosis: the layer stays open
                // and nothing is pushed on it.
                Err((open, _)) => (Handle::Open(open), Err(AnalyzerError::ForkInUse)),
            },
            Handle::Taken => (Handle::Taken, Err(lost("a level lost its layer"))),
        };
        self.handle = handle;
        sealed
    }
}

impl RaSession {
    /// The counters of each level's layer of the live fork, the lowest level first (an
    /// `Analyzer` fork has one level, whose layer is open); empty with no fork. Reads counters
    /// only, runs no query, changes nothing. The Layers of `Forks` give theirs through
    /// [`crate::RaLayer::stats`].
    ///
    /// # Contract
    /// - C1: one row per level, in level order, none with no level; a level whose layer ran its
    ///   diagnosis has memos.
    ///   tested: tests/check.rs level_stats_has_a_row_per_engine_level
    pub fn level_stats(&self) -> Vec<LayerStats> {
        self.stack
            .iter()
            .filter_map(|level| match &level.layer.handle {
                Handle::Open(open) => Some(open.stats()),
                Handle::Sealed(sealed) => Some(sealed.stats()),
                Handle::Taken => None,
            })
            .collect()
    }
}

pub(crate) fn engine_error(e: EngineError) -> AnalyzerError {
    match e {
        // Neither can happen to a layer the adapter just opened and has not lent; both are a
        // handle the adapter let escape, which `ForkInUse` names at the ports.
        EngineError::InUse | EngineError::Lent => AnalyzerError::ForkInUse,
    }
}

fn base(s: &RaSession) -> Result<&EngineBase, AnalyzerError> {
    s.frozen
        .as_ref()
        .ok_or_else(|| lost("a fork has no frozen base"))
}

/// Freezes the base of `s` for a fork: its database moves into the Engine and `host` holds an
/// empty one. `ForkInUse` when another handle of the database is alive; the session is then as
/// it was.
pub(crate) fn freeze(s: &mut RaSession) -> Result<(), AnalyzerError> {
    let spare = s.spare_db.take().unwrap_or_default();
    let db = std::mem::replace(s.host.raw_database_mut(), spare);
    match Engine::try_freeze(db) {
        Ok(frozen) => {
            frozen.adopt_created(s.created_in_db.iter().copied());
            let refused_at_freeze = frozen.refused_writes();
            s.frozen = Some(EngineBase {
                frozen,
                refused_at_freeze,
                sampled_from: None,
                vfs_created: BTreeSet::new(),
            });
            Ok(())
        }
        Err((db, _)) => {
            s.spare_db = Some(std::mem::replace(s.host.raw_database_mut(), db));
            Err(AnalyzerError::ForkInUse)
        }
    }
}

/// Drops every level left, checks that the fork's layers never wrote the base (I2: no write the
/// frozen base refused), thaws the base and puts its database back into `host`, and drops the
/// vfs entries of every file created while it was frozen. A base that was written, or that
/// cannot be thawed, poisons the session and is the failure.
fn thaw(s: &mut RaSession) -> Result<(), RevertFailure> {
    s.stack.clear();
    let Some(mut base) = s.frozen.take() else {
        return Ok(());
    };
    for rel in std::mem::take(&mut base.vfs_created) {
        fork::vfs_drop(s, &rel);
    }
    s.created_in_db.extend(base.frozen.created_files());
    let refused = base
        .frozen
        .refused_writes()
        .saturating_sub(base.refused_at_freeze);
    match base.frozen.thaw() {
        Ok(db) => {
            s.spare_db = Some(std::mem::replace(s.host.raw_database_mut(), db));
        }
        Err((frozen, _)) => {
            s.frozen = Some(EngineBase { frozen, ..base });
            let msg = "the base cannot be thawed: a handle of it outlived the fork".to_owned();
            s.poisoned.get_or_insert(msg.clone());
            return Err(RevertFailure::Poisoned(msg));
        }
    }
    if refused > 0 {
        let msg = format!("a layer wrote the base: {refused} write(s) refused (I2)");
        s.poisoned.get_or_insert(msg.clone());
        return Err(RevertFailure::Poisoned(msg));
    }
    Ok(())
}

/// The writes the frozen base refused since the freeze (0 with no frozen base): a rise is a
/// layer that wrote the base (I2).
pub(crate) fn refused_since_freeze(s: &RaSession) -> usize {
    s.frozen.as_ref().map_or(0, |base| {
        base.frozen
            .refused_writes()
            .saturating_sub(base.refused_at_freeze)
    })
}

/// Thaws the base when nothing of it is held any more, and then puts its database back into
/// `host` and drops the vfs entries of the files created while it was frozen: true. With a layer
/// or another handle of the base alive it changes nothing and is false, so it can be tried again
/// (`Forks::close` C8); it never poisons the session. True with no frozen base.
pub(crate) fn try_thaw(s: &mut RaSession) -> bool {
    let Some(base) = s.frozen.take() else {
        return true;
    };
    let EngineBase {
        frozen,
        refused_at_freeze,
        sampled_from,
        vfs_created,
    } = base;
    let created = frozen.created_files();
    match frozen.thaw() {
        Ok(db) => {
            s.created_in_db.extend(created);
            s.spare_db = Some(std::mem::replace(s.host.raw_database_mut(), db));
            for rel in vfs_created {
                fork::vfs_drop(s, &rel);
            }
            true
        }
        Err((frozen, _)) => {
            s.frozen = Some(EngineBase {
                frozen,
                refused_at_freeze,
                sampled_from,
                vfs_created,
            });
            false
        }
    }
}

/// Runs `f` with a host of the state of the top level: the session's own host when no fork is
/// live; else a host over the top level's layer while it is open, over a throwaway
/// child of it once it is sealed, or over a throwaway child of the base with no level. The
/// host lives only for `f`. A poisoned top level is `Panicked` with its poison, and `f` does not
/// run; a handle that outlives a throwaway child is `ForkInUse`.
pub(crate) fn with_top_host<R>(
    s: &RaSession,
    f: impl FnOnce(&AnalysisHost) -> R,
) -> Result<R, AnalyzerError> {
    let Some(base) = &s.frozen else {
        return Ok(f(&s.host));
    };
    let on = |db: &RootDatabase| {
        let host = AnalysisHost::with_database(db.clone());
        f(&host)
    };
    match s.stack.last().map(|level| &level.layer) {
        None => base.frozen.root().read(on).map_err(engine_error),
        Some(layer) => {
            if let Some(msg) = layer.poison() {
                return Err(AnalyzerError::Panicked(format!(
                    "the level is poisoned by a panic in its layer: {msg}"
                )));
            }
            match &layer.handle {
                Handle::Open(open) => Ok(on(&open.db())),
                Handle::Sealed(sealed) => sealed.read(on).map_err(engine_error),
                Handle::Taken => Err(lost("a level lost its layer")),
            }
        }
    }
}

/// Records the panic of a diagnosis that ran in the top level's open layer: the layer is
/// poisoned. A panic in a throwaway child poisons only the child, which is gone.
pub(crate) fn poison_top_if_open(s: &RaSession, msg: &str) {
    if let Some(layer) = s.stack.last().map(|l| &l.layer)
        && matches!(layer.handle, Handle::Open(_))
    {
        layer.set_poison(msg);
    }
}

/// The text of `id` at the top level (the base with no level). `Files::file_text` panics on an
/// unknown id, so the read is wrapped.
pub(crate) fn top_text(s: &RaSession, id: FileId) -> Result<String, AnalyzerError> {
    let top = s.stack.last().map(|level| &level.layer);
    let base = base(s)?;
    catch_unwind(AssertUnwindSafe(|| {
        let text = match top.map(|layer| &layer.handle) {
            None => base.frozen.root().text(id),
            Some(Handle::Open(open)) => open.text(id),
            Some(Handle::Sealed(sealed)) => sealed.text(id),
            Some(Handle::Taken) => return Err(lost("a level lost its layer")),
        };
        Ok(text.to_string())
    }))
    .map_err(|p| AnalyzerError::Panicked(panic_message(p)))?
}

/// The text of `id` at the base, read through the frozen base.
pub(crate) fn base_text(s: &RaSession, id: FileId) -> Result<String, AnalyzerError> {
    let base = base(s)?;
    catch_unwind(AssertUnwindSafe(|| base.frozen.root().text(id).to_string()))
        .map_err(|p| AnalyzerError::Panicked(panic_message(p)))
}

/// Resolves `edits` (whose paths are `edited`, from `fork::edited_paths`) against the top level,
/// seals it, writes them into a new layer on it and pushes the level; `sampled` makes its pop
/// check the isolation of the base. A refused write (`Edit`) or a write that panicked
/// (`Panicked`; the new layer is poisoned and dropped) pushes no level and leaves the levels
/// below as they were; neither poisons the session.
pub(crate) fn push_level(
    a: &RaAnalyzer,
    s: &mut RaSession,
    edits: &[Edit],
    edited: Vec<RelPath>,
    sampled: bool,
) -> Result<(), AnalyzerError> {
    let write_started_ms = a.clock.now_ms();
    if let Some(top) = s.stack.last()
        && let Some(msg) = top.layer.poison()
    {
        return Err(AnalyzerError::Panicked(format!(
            "the level below is poisoned by a panic in its layer: {msg}"
        )));
    }
    let pure = fork::resolve_pure_at_top(s, edits);
    let actions = fork::complete_on_session(s, pure)?;
    if sampled {
        let base = s
            .frozen
            .as_mut()
            .ok_or_else(|| lost("a fork has no frozen base"))?;
        if base.sampled_from.is_none() {
            base.sampled_from = Some(base.frozen.isolation());
        }
    }
    let below = fork::top_state(s).cloned();
    let parent = match s.stack.last_mut() {
        Some(top) => top.layer.seal()?,
        None => base(s)?.frozen.root(),
    };
    let mut child = parent.child();
    drop(parent);

    let mut state = LayerState::new(edited, below);
    let inject = a.db_write_panic == Some(DbWritePanic::Write);
    let written = catch_unwind(AssertUnwindSafe(|| {
        write_actions(&mut state, &mut child, actions, inject)
    }));
    let failed = match written {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e),
        Err(p) => Some(AnalyzerError::Panicked(panic_message(p))),
    };
    if let Some(e) = failed {
        drop(child);
        return Err(e);
    }

    let parent_state = state.parent.clone();
    let live_below = parent_state
        .as_deref()
        .map_or(s.files.as_slice(), |p| p.live.as_slice());
    fork::finish_level(live_below, &s.rev_deps, &mut state);
    let layer = LevelLayer {
        handle: Handle::Open(child),
        poisoned: Mutex::new(None),
        sampled,
    };
    let write_ms = a.clock.now_ms().saturating_sub(write_started_ms);
    s.stack.push(Level::layered(state, layer, write_ms));
    Ok(())
}

/// Writes the resolved actions into `child`, recording created, revived and deleted files in
/// `state` as each is made. `inject` makes the write panic first (the test hook
/// `DbWritePanic::Write`). A deleted file loses its text and leaves its source root, as in the
/// flat reference check; a created or revived file enters the source root of its sibling or its
/// own.
pub(crate) fn write_actions(
    state: &mut LayerState,
    child: &mut OpenLayer<RootDatabase>,
    actions: Vec<Action>,
    inject: bool,
) -> Result<(), AnalyzerError> {
    if inject {
        panic!("injected database write panic");
    }
    let mut added: Vec<Action> = Vec::new();
    for action in actions {
        match action {
            Action::Modify { id, new, .. } => child.write(id, new).map_err(engine_error)?,
            Action::Delete { id, rel, below } => state.deleted.push((id, rel, below)),
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
                let root_id = child.source_root_of(sibling);
                state.created.push((id, rel));
                let root = fork::root_with(&child.source_root(root_id), Some((id, path)), None);
                child.set_source_root(root_id, root).map_err(engine_error)?;
                child
                    .create_in_root(id, new, root_id)
                    .map_err(engine_error)?;
            }
            Action::Revive { id, rel, new, path } => {
                let root_id = child.source_root_of(id);
                let root = fork::root_with(&child.source_root(root_id), Some((id, path)), None);
                child.set_source_root(root_id, root).map_err(engine_error)?;
                child.write(id, new).map_err(engine_error)?;
                state.revived.push((id, rel));
            }
            Action::Modify { .. } | Action::Delete { .. } => {}
        }
    }
    for (id, _, _) in &state.deleted {
        let root_id: SourceRootId = child.source_root_of(*id);
        let root: TriArc<SourceRoot> =
            fork::root_with(&child.source_root(root_id), None, Some(*id));
        child.set_source_root(root_id, root).map_err(engine_error)?;
        child.write(*id, "").map_err(engine_error)?;
    }
    Ok(())
}

/// Whether the base is as the fork's first sampled write found it: no write refused since the
/// freeze, and every slot of its fingerprint unchanged. The isolation check of a sampled pop;
/// `a.reassert_fault` makes it fail to run.
fn base_kept(a: &RaAnalyzer, s: &RaSession) -> Result<bool, AnalyzerError> {
    if a.reassert_fault {
        return Err(AnalyzerError::Panicked("injected re-assert fault".into()));
    }
    let base = base(s)?;
    let now = base.frozen.isolation();
    let refused = now.refused.saturating_sub(base.refused_at_freeze) > 0;
    let kept = base
        .sampled_from
        .as_ref()
        .is_none_or(|from| from.fingerprint.preserved_in(&now.fingerprint).is_ok());
    Ok(!refused && kept)
}

/// Pops the top level: drops its layer and returns `true`; with no level it is `false`. A
/// sampled level checks the isolation of the base (I2); nothing was written to revert. A base that changed poisons the session and is `Mismatch` of the level's paths, and a
/// check that cannot run poisons it too. An `Event::Reverted` records the pop unless the check
/// could not run.
pub(crate) fn pop_level(a: &RaAnalyzer, s: &mut RaSession) -> Result<bool, RevertFailure> {
    let Some(level) = s.stack.pop() else {
        return Ok(false);
    };
    let t = a.clock.now_ms();
    let sampled = level.layer.sampled;
    let edited = level.state.edited.clone();
    drop(level);
    let revert_ok = if sampled {
        match base_kept(a, s) {
            Ok(kept) => Some(kept),
            Err(e) => {
                let msg = s
                    .poisoned
                    .get_or_insert_with(|| {
                        format!("the isolation check after a pop could not run: {e}")
                    })
                    .clone();
                return Err(RevertFailure::Poisoned(msg));
            }
        }
    } else {
        None
    };
    a.observer.observe(Event::Reverted {
        revert_ms: a.clock.now_ms().saturating_sub(t),
        revert_ok,
    });
    if revert_ok == Some(false) {
        s.poisoned = Some("a layer changed the base (I2)".to_owned());
        return Err(RevertFailure::Mismatch(edited));
    }
    Ok(true)
}

/// Ends the fork: pops every level, top first, under the rules of
/// [`pop_level`], then thaws the base. A poisoned session pops nothing (the levels are dropped)
/// and is `Poisoned`; the first failure stops the pops, the remaining levels are dropped and that
/// failure is the result. The base is thawed whatever happens.
pub(crate) fn release_levels(a: &RaAnalyzer, s: &mut RaSession) -> Result<(), RevertFailure> {
    s.live = None;
    let popped = match s.poisoned.clone() {
        Some(msg) => Err(RevertFailure::Poisoned(msg)),
        None => (|| {
            while pop_level(a, s)? {}
            Ok(())
        })(),
    };
    let thawed = thaw(s);
    s.base_texts.clear();
    popped.and(thawed)
}
