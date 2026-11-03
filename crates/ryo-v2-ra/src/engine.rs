//! The forks of a session on the fork Engine (D11, stage 3; `RaConfig.engine`).
//!
//! While a fork is live the session's database is the frozen base of the Engine and `host`
//! holds an empty one. A level is a layer: its write goes into a new layer on the level below
//! (or on the base), which seals the level below; its diagnosis runs in the layer while the
//! layer is open, and in a throwaway child of it once it is sealed (E9). `pop` drops the top
//! layer and `release` drops them all and thaws the base, so nothing writes the base: the
//! check that a level's write left the base as it was is the isolation of the Engine (I2), not
//! a revert.
//!
//! A panic inside a layer (its write, or a query of its diagnosis) poisons that layer and every
//! layer above it, never the base, a level below or the session: a write that panics pushes no
//! level, and a level whose diagnosis panicked refuses every further diagnosis and every write
//! on it until it is popped. The session is poisoned only when the base was written (a write the
//! frozen base refused, or a fingerprint of the base that changed) or the isolation check could
//! not run.

use crate::diagnostics::panic_message;
use crate::fork::{self, Action, Level, RevertFailure};
use crate::session::RaSession;
use crate::{DbWritePanic, RaAnalyzer};
use ra_ap_base_db::{SourceRoot, SourceRootId};
use ra_ap_ide::AnalysisHost;
use ra_ap_ide_db::RootDatabase;
use ra_ap_vfs::FileId;
use ryo_v2_core::app::ports::{AnalyzerError, Event};
use ryo_v2_core::domain::{Edit, RelPath};
use ryo_v2_engine::{Engine, EngineError, Frozen, Isolation, OpenLayer, SealedLayer};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Mutex, PoisonError};
use triomphe::Arc as TriArc;

/// The frozen base of a live fork and what its isolation check compares with.
pub(crate) struct EngineBase {
    pub(crate) frozen: Frozen<RootDatabase>,
    /// The base's refused writes at the freeze (the count is kept for the database's life).
    refused_at_freeze: usize,
    /// The isolation of the base when the fork's first sampled write was made: every later
    /// sampled check compares the base with it.
    sampled_from: Option<Isolation>,
}

/// The layer of a level, open until a write pushes the next level on it.
pub(crate) struct LevelLayer {
    state: LayerState,
    /// The panic that poisoned the layer, if one did.
    poisoned: Mutex<Option<String>>,
    /// Whether the write of the level was sampled: its `pop` checks the isolation of the base.
    sampled: bool,
}

enum LayerState {
    Open(OpenLayer<RootDatabase>),
    Sealed(SealedLayer<RootDatabase>),
    /// Only inside [`LevelLayer::seal`], which always puts the layer back.
    Taken,
}

/// A level of a fork on the Engine without its layer, or a fork on the Engine without its frozen
/// base: a fault of the adapter, which no port operation leads to.
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
        let (state, sealed) = match std::mem::replace(&mut self.state, LayerState::Taken) {
            LayerState::Sealed(sealed) => (LayerState::Sealed(sealed.clone()), Ok(sealed)),
            LayerState::Open(open) => match open.seal() {
                Ok(sealed) => (LayerState::Sealed(sealed.clone()), Ok(sealed)),
                // A clone of the layer's database outlived a diagnosis: the layer stays open
                // and nothing is pushed on it.
                Err((open, _)) => (LayerState::Open(open), Err(AnalyzerError::ForkInUse)),
            },
            LayerState::Taken => (LayerState::Taken, Err(lost("a level lost its layer"))),
        };
        self.state = state;
        sealed
    }
}

/// The level's layer; every level of a fork on the Engine has one.
fn layer_of(level: &Level) -> Result<&LevelLayer, AnalyzerError> {
    level
        .layer
        .as_ref()
        .ok_or_else(|| lost("a level has no layer"))
}

fn engine_error(e: EngineError) -> AnalyzerError {
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
/// frozen base refused), thaws the base and puts its database back into `host`. A base that was
/// written, or that cannot be thawed, poisons the session and is the failure.
fn thaw(s: &mut RaSession) -> Result<(), RevertFailure> {
    for level in std::mem::take(&mut s.stack) {
        fork::drop_created(s, &level);
    }
    let Some(base) = s.frozen.take() else {
        return Ok(());
    };
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

/// Runs `f` with a host of the state of the top level: the session's own host when no fork is
/// on the Engine; else a host over the top level's layer while it is open, over a throwaway
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
    match s.stack.last().map(layer_of).transpose()? {
        None => base.frozen.root().read(on).map_err(engine_error),
        Some(layer) => {
            if let Some(msg) = layer.poison() {
                return Err(AnalyzerError::Panicked(format!(
                    "the level is poisoned by a panic in its layer: {msg}"
                )));
            }
            match &layer.state {
                LayerState::Open(open) => Ok(on(&open.db())),
                LayerState::Sealed(sealed) => sealed.read(on).map_err(engine_error),
                LayerState::Taken => Err(lost("a level lost its layer")),
            }
        }
    }
}

/// Records the panic of a diagnosis that ran in the top level's open layer: the layer is
/// poisoned. A panic in a throwaway child poisons only the child, which is gone.
pub(crate) fn poison_top_if_open(s: &RaSession, msg: &str) {
    if let Some(layer) = s.stack.last().and_then(|l| l.layer.as_ref())
        && matches!(layer.state, LayerState::Open(_))
    {
        layer.set_poison(msg);
    }
}

/// The text of `id` at the top level (the base with no level). `Files::file_text` panics on an
/// unknown id, so the read is wrapped.
pub(crate) fn top_text(s: &RaSession, id: FileId) -> Result<String, AnalyzerError> {
    let top = s.stack.last().map(layer_of).transpose()?;
    let base = base(s)?;
    catch_unwind(AssertUnwindSafe(|| {
        let text = match top.map(|layer| &layer.state) {
            None => base.frozen.root().text(id),
            Some(LayerState::Open(open)) => open.text(id),
            Some(LayerState::Sealed(sealed)) => sealed.text(id),
            Some(LayerState::Taken) => return Err(lost("a level lost its layer")),
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

/// `fork::push_level` on the Engine: resolves `edits` against the top level, seals it, writes
/// them into a new layer on it and pushes the level. A refused write (`Edit`) or a write that
/// panicked (`Panicked`; the new layer is poisoned and dropped) pushes no level and leaves the
/// levels below as they were; neither poisons the session.
pub(crate) fn push_level(
    a: &RaAnalyzer,
    s: &mut RaSession,
    edits: &[Edit],
    edited: Vec<RelPath>,
    sampled: bool,
) -> Result<(), AnalyzerError> {
    let write_started_ms = a.clock.now_ms();
    if let Some(top) = s.stack.last()
        && let Some(msg) = layer_of(top)?.poison()
    {
        return Err(AnalyzerError::Panicked(format!(
            "the level below is poisoned by a panic in its layer: {msg}"
        )));
    }
    let actions = fork::resolve(s, edits)?;
    if sampled {
        let base = s
            .frozen
            .as_mut()
            .ok_or_else(|| lost("a fork has no frozen base"))?;
        if base.sampled_from.is_none() {
            base.sampled_from = Some(base.frozen.isolation());
        }
    }
    let parent = match s.stack.last_mut() {
        Some(top) => top
            .layer
            .as_mut()
            .ok_or_else(|| lost("a level has no layer"))?
            .seal()?,
        None => base(s)?.frozen.root(),
    };
    let mut child = parent.child();
    drop(parent);

    let mut level = Level::new(edited);
    let inject = a.db_write_panic == Some(DbWritePanic::Write);
    let written = catch_unwind(AssertUnwindSafe(|| {
        write_actions(s, &mut level, &mut child, actions, inject)
    }));
    let failed = match written {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e),
        Err(p) => Some(AnalyzerError::Panicked(panic_message(p))),
    };
    if let Some(e) = failed {
        drop(child);
        fork::drop_created(s, &level);
        return Err(e);
    }

    fork::finish_level(s, &mut level);
    level.layer = Some(LevelLayer {
        state: LayerState::Open(child),
        poisoned: Mutex::new(None),
        sampled,
    });
    level.write_ms = a.clock.now_ms().saturating_sub(write_started_ms);
    s.stack.push(level);
    Ok(())
}

/// Writes the resolved actions into `child`, recording created, revived and deleted files in
/// `level` as each is made. `inject` makes the write panic first (the test hook
/// `DbWritePanic::Write`). A deleted file loses its text and leaves its source root, as on the
/// undo log; a created or revived file enters the source root of its sibling or its own.
fn write_actions(
    s: &mut RaSession,
    level: &mut Level,
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
            Action::Delete { id, rel, below } => level.deleted.push((id, rel, below)),
            Action::Create { .. } | Action::Revive { .. } => added.push(action),
        }
    }
    for action in added {
        match action {
            Action::Create { rel, new } => {
                let sibling = fork::sibling_file(s, &rel)?;
                let root_id = child.source_root_of(sibling);
                let (id, path) = fork::vfs_create(s, &rel, &new)?;
                level.created.push((id, rel));
                let root = fork::root_with(&child.source_root(root_id), Some((id, path)), None);
                child.set_source_root(root_id, root).map_err(engine_error)?;
                child
                    .create_in_root(id, new, root_id)
                    .map_err(engine_error)?;
            }
            Action::Revive { id, rel, new } => {
                let path = s.vfs.file_path(id).clone();
                let root_id = child.source_root_of(id);
                let root = fork::root_with(&child.source_root(root_id), Some((id, path)), None);
                child.set_source_root(root_id, root).map_err(engine_error)?;
                child.write(id, new).map_err(engine_error)?;
                level.revived.push((id, rel));
            }
            Action::Modify { .. } | Action::Delete { .. } => {}
        }
    }
    for (id, _, _) in &level.deleted {
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

/// `fork::pop_level` on the Engine: drops the top layer and returns `true`; with no level it is
/// `false`. A sampled level checks the isolation of the base (I2) instead of re-diagnosing a
/// revert; a base that changed poisons the session and is `Mismatch` of the level's paths, and a
/// check that cannot run poisons it too. An `Event::Reverted` records the pop unless the check
/// could not run.
pub(crate) fn pop_level(a: &RaAnalyzer, s: &mut RaSession) -> Result<bool, RevertFailure> {
    let Some(level) = s.stack.pop() else {
        return Ok(false);
    };
    let t = a.clock.now_ms();
    let sampled = level.layer.as_ref().is_some_and(|l| l.sampled);
    fork::drop_created(s, &level);
    let edited = level.edited.clone();
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

/// `fork::release_levels` on the Engine: pops every level, top first, under the rules of
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
