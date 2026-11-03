// ryo-v2 fork: the fork layer of ryo-v2's in-memory fork Engine (D11). Not upstream salsa code.
//! Layers over a frozen base.
//!
//! A *base* is a salsa database that has been [frozen](freeze): from then on nothing writes it
//! except the shared writes counted in [`SharedWrites`]. A [`Layer`] is one write on top of its
//! parent (the base, or a sealed layer): it holds the input values it set, under a revision taken
//! from a process-wide counter, so along a chain `base < L1 < ... < Lk` the revisions increase and
//! the chain is a linear history. A database handle carries at most one layer, in its
//! [`ZalsaView`]; every read the handle makes resolves through that layer's chain, and every write
//! goes to it.
//!
//! What is routed through the chain so far (step 2a of stage 2): the handle's current revision and
//! last-changed revisions, input field reads (getters and the debug read) and input field writes
//! ([`set_input_field`]), and input creation (a shared slot allocation). Tracked functions,
//! tracked structs, interned values, accumulators and `specify` are not routed yet: under a layer
//! they panic with "not yet supported under a layer" before they read or write anything. A
//! handle without a layer behaves as upstream salsa.
//!
//! Invariants enforced here, each a panic in release builds too:
//!
//! - a handle with a layer never takes `zalsa_mut` (a setter, `synthetic_write`, LRU eviction,
//!   `memory_usage`, ...): it panics instead of waiting for other handles;
//! - a base is frozen only through its only handle, so no write of the base is in flight;
//! - a frozen base takes no write through `zalsa_mut`, no memo insert, no `verified_at`,
//!   `verified_final`, accumulated-inputs or cycle-iteration store on a memo, no tracked-struct
//!   creation, update or deletion, no interning and no enumeration of interned values, and no
//!   input creation except the counted one under a layer, from any handle;
//! - a sealed layer takes no write (a seal and a write into the layer do not interleave), and
//!   only a sealed layer gets a child;
//! - a layer's cancellation is final: no handle's `attach` scope resets the shared token;
//! - a layer of another base cannot be set on a handle;
//! - a layered input write is refused for a field whose durability is above `LOW`.

mod view;

use std::any::Any;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use rustc_hash::FxHashMap;

pub use self::view::ZalsaView;
use crate::input::Configuration as InputConfiguration;
use crate::storage::HasStorage;
use crate::sync::{Arc, Mutex, MutexGuard};
use crate::zalsa::{IngredientIndex, Zalsa};
use crate::zalsa_local::CancellationToken;
use crate::{Database, Durability, Id, Revision};

/// The process-wide layer revision counter: the revision of the newest layer handed out, or the
/// highest base revision a [`freeze`] raised it to. Every new layer takes the next value, so a
/// layer's revision exceeds its parent's and every revision any frozen base had at its freeze.
static LAYER_REVISION: AtomicUsize = AtomicUsize::new(0);

/// Source of the identities that tie a layer to its base.
static BASE_ID: AtomicU64 = AtomicU64::new(1);

/// Takes the next layer revision.
fn next_layer_revision() -> Revision {
    Revision::from(LAYER_REVISION.fetch_add(1, Ordering::SeqCst) + 1)
}

/// The highest layer revision handed out so far (or the floor a freeze raised the counter to).
pub fn highest_layer_revision() -> Option<Revision> {
    Revision::from_opt(LAYER_REVISION.load(Ordering::SeqCst))
}

/// The fork state of one [`Zalsa`]: its identity as a base, whether it is frozen, and the
/// isolation counters of the shared writes that layers make to it.
pub(crate) struct ForkState {
    id: u64,
    frozen: AtomicBool,
    slot_allocations: AtomicUsize,
}

impl ForkState {
    pub(crate) fn new() -> Self {
        ForkState {
            id: BASE_ID.fetch_add(1, Ordering::Relaxed),
            frozen: AtomicBool::new(false),
            slot_allocations: AtomicUsize::new(0),
        }
    }

    #[inline]
    pub(crate) fn is_frozen(&self) -> bool {
        self.frozen.load(Ordering::Acquire)
    }

    /// Panics if the base is frozen: `what` names the write that reached it.
    #[inline]
    pub(crate) fn assert_not_frozen(&self, what: &str) {
        if self.is_frozen() {
            frozen_base_write(what);
        }
    }

    /// Counts one shared write of the kind `kind` made under a layer.
    pub(crate) fn record_shared_write(&self, kind: SharedWriteKind) {
        match kind {
            SharedWriteKind::SlotAllocation => {
                self.slot_allocations.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[cold]
#[inline(never)]
#[track_caller]
fn frozen_base_write(what: &str) -> ! {
    panic!("ryo-v2 fork: write to a frozen base ({what})")
}

/// The kinds of shared write a layer may make to its base (D11, E2). Every other write to a
/// frozen base panics.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum SharedWriteKind {
    /// A table slot allocated under a layer: so far only for an input created under a layer.
    SlotAllocation,
}

/// The isolation counters of a base: how many shared writes of each listed kind layers made
/// to it since it was created. A base write of any other kind under a frozen base panics, so it
/// has no counter.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SharedWrites {
    /// Table slots allocated under a layer (an input created through a layered handle).
    pub slot_allocations: usize,
}

/// One input struct's field values as a layer sees them: the fields, and per field its revision
/// and durability. Kept for the life of the layer; a later write in the same layer adds a new
/// entry and leaves this one in place, so a reference handed out from it stays valid.
pub(crate) struct InputEntry<C: InputConfiguration> {
    pub(crate) fields: C::Fields,
    pub(crate) revisions: Box<[Revision]>,
    pub(crate) durabilities: Box<[Durability]>,
}

/// One write on top of a parent: a revision, the parent chain, a sealed flag, a cancellation
/// token, and the input values it set.
pub struct Layer {
    base: u64,
    revision: Revision,
    parent: Option<Arc<Layer>>,
    sealed: AtomicBool,
    written: AtomicBool,
    /// Held by an input write for its whole read-modify-insert and by [`Layer::seal`]: a write
    /// and a seal do not interleave, and two writes to one input in one layer lose neither.
    write: Mutex<()>,
    token: CancellationToken,
    /// `(input struct ingredient, id)` to the index of the newest entry in `values`.
    inputs: Mutex<FxHashMap<(IngredientIndex, Id), usize>>,
    /// Append-only: an entry stays until the layer is dropped.
    values: boxcar::Vec<Box<dyn Any + Send + Sync>>,
}

/// The lock around `inputs` is held only for one map lookup or insert, which runs no user code
/// and cannot panic halfway, and `values` is append-only. The `write` lock guards no data: a
/// setter that panics under it unwinds before the insert, so the layer keeps its old entry. A
/// panic elsewhere leaves no torn state, as upstream argues for [`Zalsa`].
impl std::panic::RefUnwindSafe for Layer {}

impl std::fmt::Debug for Layer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Layer")
            .field("revision", &self.revision)
            .field("parent", &self.parent.as_ref().map(|p| p.revision))
            .field("sealed", &self.is_sealed())
            .field("written", &self.has_writes())
            .finish_non_exhaustive()
    }
}

impl Layer {
    fn new(base: u64, parent: Option<Arc<Layer>>) -> Arc<Layer> {
        Arc::new(Layer {
            base,
            revision: next_layer_revision(),
            parent,
            sealed: AtomicBool::new(false),
            written: AtomicBool::new(false),
            write: Mutex::new(()),
            token: CancellationToken::default(),
            inputs: Mutex::new(FxHashMap::default()),
            values: boxcar::Vec::new(),
        })
    }

    /// The layer's own revision: unique in the process, above its parent's.
    pub fn revision(&self) -> Revision {
        self.revision
    }

    /// The parent layer, `None` for a layer directly on the base.
    pub fn parent(&self) -> Option<&Arc<Layer>> {
        self.parent.as_ref()
    }

    /// Whether the layer is sealed: it takes no further write and may have children.
    pub fn is_sealed(&self) -> bool {
        self.sealed.load(Ordering::Acquire)
    }

    /// Seals the layer. Idempotent. Waits for an input write in progress on the layer, so a
    /// write either completes before the seal or is refused after it.
    pub fn seal(&self) {
        let _write = self.write.lock();
        self.sealed.store(true, Ordering::Release);
    }

    /// Whether the layer itself has set an input.
    pub fn has_writes(&self) -> bool {
        self.written.load(Ordering::Acquire)
    }

    /// The layer's cancellation token, shared by every handle that carries the layer.
    /// Cancelling it stops the queries of the layer and of its descendants. A layer's
    /// cancellation is final: unlike a base handle's token, no handle resets it when its
    /// `attach` scope ends, since that would drop the cancellation for every other handle of the
    /// layer and of its descendants.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// The layers of the chain, this one first, then its ancestors.
    pub(crate) fn chain(&self) -> impl Iterator<Item = &Layer> {
        std::iter::successors(Some(self), |l| l.parent.as_deref())
    }

    /// The revision of the newest layer of the chain that has writes, if any.
    pub(crate) fn last_written(&self) -> Option<Revision> {
        self.chain().find(|l| l.has_writes()).map(|l| l.revision)
    }

    /// Whether an ancestor's token (not this layer's own) was cancelled.
    pub(crate) fn ancestor_cancelled(&self) -> bool {
        self.chain().skip(1).any(|l| l.token.is_cancelled())
    }

    /// The newest values of input `id` of ingredient `ingredient` on the chain.
    pub(crate) fn input<C: InputConfiguration>(
        &self,
        ingredient: IngredientIndex,
        id: Id,
    ) -> Option<&InputEntry<C>> {
        for layer in self.chain() {
            let index = layer.inputs.lock().get(&(ingredient, id)).copied();
            if let Some(index) = index {
                let entry = layer
                    .values
                    .get(index)
                    .expect("ryo-v2 fork: an input entry index names a pushed value");
                return Some(
                    entry
                        .downcast_ref::<InputEntry<C>>()
                        .expect("ryo-v2 fork: an input entry has its ingredient's type"),
                );
            }
        }
        None
    }

    /// Takes the layer's write lock for an input write into it, after refusing a sealed layer.
    /// The write must not write into this layer again while it holds the guard (the lock is not
    /// reentrant).
    #[track_caller]
    pub(crate) fn begin_write(&self, what: &str) -> MutexGuard<'_, ()> {
        let write = self.write.lock();
        self.assert_open(what);
        write
    }

    /// Stores new values of input `id` in this layer. Panics on a sealed layer.
    pub(crate) fn insert_input<C: InputConfiguration>(
        &self,
        ingredient: IngredientIndex,
        id: Id,
        entry: InputEntry<C>,
    ) {
        self.assert_open("an input write");
        let index = self.values.push(Box::new(entry));
        self.inputs.lock().insert((ingredient, id), index);
        self.written.store(true, Ordering::Release);
    }

    /// Panics if the layer is sealed: `what` names the write that reached it.
    #[track_caller]
    pub(crate) fn assert_open(&self, what: &str) {
        if self.is_sealed() {
            panic!("ryo-v2 fork: write to a sealed layer ({what})");
        }
    }
}

/// Panics: `what` is not routed through a layer yet, and a layered handle reached it.
#[cold]
#[inline(never)]
#[track_caller]
pub(crate) fn not_yet_supported(what: &str) -> ! {
    panic!("ryo-v2 fork: not yet supported under a layer: {what}")
}

/// Freezes the base `db` belongs to and returns its revision. From here on the base takes no
/// write except the shared writes of layers, and the layer revision counter is at least the base
/// revision, so every layer made after this call has a revision above it.
///
/// Panics if `db` carries a layer, another handle of the base is alive, or the base is already
/// frozen. With `db` the only handle, and borrowed here, no query of the base is running and no
/// `zalsa_mut` is waiting, so none can write the base past the frozen flag it checked earlier.
pub fn freeze<Db: HasStorage>(db: &Db) -> Revision {
    let view = db.storage().fork_view();
    assert!(
        view.layer().is_none(),
        "ryo-v2 fork: freeze takes a handle of the base, not a layered handle"
    );
    let handles = db.storage().handle_count();
    assert!(
        handles == 1,
        "ryo-v2 fork: freeze needs the only handle of the base ({handles} are alive)"
    );
    let state = view.fork();
    assert!(
        !state.frozen.swap(true, Ordering::AcqRel),
        "ryo-v2 fork: the base is already frozen"
    );
    let base_revision = view.base_revision();
    LAYER_REVISION.fetch_max(base_revision.as_usize(), Ordering::SeqCst);
    base_revision
}

/// Whether the base `db` belongs to is frozen.
pub fn is_frozen<Db: HasStorage>(db: &Db) -> bool {
    db.storage().fork_view().fork().is_frozen()
}

/// A new write-free layer directly on the frozen base of `db`.
///
/// Panics if `db` carries a layer or its base is not frozen.
pub fn root_layer<Db: HasStorage>(db: &Db) -> Arc<Layer> {
    let view = db.storage().fork_view();
    assert!(
        view.layer().is_none(),
        "ryo-v2 fork: a root layer is made from a handle of the base"
    );
    assert!(
        view.fork().is_frozen(),
        "ryo-v2 fork: layers need a frozen base"
    );
    Layer::new(view.fork().id, None)
}

/// A new open layer on `parent`.
///
/// Panics if `parent` is not sealed.
pub fn child_layer(parent: &Arc<Layer>) -> Arc<Layer> {
    assert!(
        parent.is_sealed(),
        "ryo-v2 fork: only a sealed layer gets a child"
    );
    Layer::new(parent.base, Some(Arc::clone(parent)))
}

/// Sets `layer` on the handle `db`: from here on its reads resolve through the layer's chain,
/// its input writes go to the layer, and it uses the layer's cancellation token. Clones of the
/// handle keep the layer.
///
/// Panics if `db` already carries a layer, its base is not frozen, or `layer` descends from
/// another base.
pub fn set_layer<Db: HasStorage>(db: &mut Db, layer: Arc<Layer>) {
    let (view, zalsa_local) = db.storage_mut().fork_parts_mut();
    assert!(
        view.layer().is_none(),
        "ryo-v2 fork: the handle already carries a layer"
    );
    assert!(
        view.fork().is_frozen(),
        "ryo-v2 fork: layers need a frozen base"
    );
    assert_eq!(
        layer.base,
        view.fork().id,
        "ryo-v2 fork: the layer descends from another base"
    );
    zalsa_local.set_cancellation_token(layer.cancellation_token());
    view.set_layer(layer);
}

/// The layer the handle `db` carries, if any.
pub fn layer_of<Db: HasStorage>(db: &Db) -> Option<&Arc<Layer>> {
    db.storage().fork_view().layer()
}

/// The number of live handles of the database `db` belongs to (clones included), read without
/// waiting.
pub fn handle_count<Db: HasStorage>(db: &Db) -> usize {
    db.storage().handle_count()
}

/// The isolation counters of the base `db` belongs to.
pub fn shared_writes<Db: HasStorage>(db: &Db) -> SharedWrites {
    let state = db.storage().fork_view().fork();
    SharedWrites {
        slot_allocations: state.slot_allocations.load(Ordering::Relaxed),
    }
}

/// Why [`try_thaw`] refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ThawRefused {
    /// The live handles of the base, this one included.
    pub handles: usize,
}

/// Thaws the frozen base of `db` when `db` is its only live handle: reserves a revision above
/// every layer revision handed out and above the base's own, in one step on the layer revision
/// counter (so no layer, made before or after, of any base, has it), moves the base to it
/// ([`Zalsa::advance_revision_past`]), and lifts the frozen flag. Returns the new base revision.
/// With any other handle alive it returns [`ThawRefused`] at once and changes nothing: it never
/// waits.
///
/// Panics if `db` carries a layer or its base is not frozen.
pub fn try_thaw<Db: HasStorage>(db: &mut Db) -> Result<Revision, ThawRefused> {
    let storage = db.storage_mut();
    let handles = storage.handle_count();
    let (view, _) = storage.fork_parts_mut();
    assert!(
        view.layer().is_none(),
        "ryo-v2 fork: thaw takes a handle of the base, not a layered handle"
    );
    assert!(
        view.fork().is_frozen(),
        "ryo-v2 fork: the base is not frozen"
    );
    if handles != 1 {
        return Err(ThawRefused { handles });
    }
    let zalsa = Arc::get_mut(view.zalsa_arc_mut())
        .expect("ryo-v2 fork: the only handle of a base holds the only reference to it");
    // Reserve `max(counter, base) + 1` on the counter itself: a layer made concurrently, of any
    // base, takes a value either below it or above it, never it.
    let base = zalsa.base_revision().as_usize();
    let previous = LAYER_REVISION
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |c| {
            Some(c.max(base) + 1)
        })
        .expect("ryo-v2 fork: the update closure always returns a value");
    let reserved = previous.max(base) + 1;
    // `reserved - 1 >= base`, so the base moves to exactly `reserved`.
    let revision = zalsa.advance_revision_past(Revision::from(reserved - 1));
    debug_assert_eq!(revision.as_usize(), reserved);
    zalsa.fork().frozen.store(false, Ordering::Release);
    Ok(revision)
}

/// Sets field `field_index` of the input `id` in the layer the handle `db` carries: the layer
/// stores the input's fields with that field changed by `setter` (which must change only that
/// field, as upstream's setters do) and the layer's revision as the field's revision. The base
/// and every other layer keep their values. `setter` runs under the layer's write lock, so it
/// may read the database but must not write into the same layer (that would deadlock).
///
/// Panics if `db` carries no layer, the layer is sealed, or the field's durability is above
/// `LOW`.
pub fn set_input_field<C, Db>(
    db: &Db,
    id: C::Struct,
    field_index: usize,
    setter: impl FnOnce(&mut C::Fields),
) where
    C: InputConfiguration,
    C::Fields: Clone,
    Db: ?Sized + Database,
{
    let view = db.zalsa();
    let layer = view
        .layer()
        .expect("ryo-v2 fork: an input write into a layer needs a layered handle");
    let ingredient = view.lookup_jar_by_type::<crate::input::JarImpl<C>>();
    let ingredient = view
        .lookup_ingredient(ingredient)
        .assert_type::<crate::input::IngredientImpl<C>>();
    ingredient.set_field_in_layer(view, layer, id, field_index, setter);
}

impl Zalsa {
    /// The base revision, for a site that a layered handle cannot reach in step 2a: each caller
    /// sits behind a "not yet supported under a layer" stub. Step 2b routes these sites through
    /// the layer and removes this accessor, so the compiler lists them again.
    #[inline]
    pub(crate) fn unrouted_revision(&self) -> Revision {
        self.base_revision()
    }

    /// The base's last-changed revision, under the same terms as [`Zalsa::unrouted_revision`].
    #[inline]
    pub(crate) fn unrouted_last_changed(&self, durability: Durability) -> Revision {
        self.base_last_changed(durability)
    }

    /// Moves the current revision to `max(current, r) + 1` and runs what a new revision runs on
    /// the ingredients ([`Zalsa::new_revision`]), without touching the last-changed revision of
    /// any durability: no base input changed. Used at thaw with `r` the highest layer revision,
    /// so the base's next state shares its revision with no layer.
    pub fn advance_revision_past(&mut self, r: Revision) -> Revision {
        self.runtime_mut().raise_current_revision(r);
        self.new_revision()
    }
}
