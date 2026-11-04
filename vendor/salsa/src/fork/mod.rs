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
//! What a layer holds, each read through the chain (the layer first, then its ancestors, then the
//! base):
//!
//! - input values it set ([`set_input_field`]), with their revisions. When the layer is sealed it
//!   also builds a *flattened view* of the inputs: per input, the newest entry the layer or an
//!   ancestor set. A sealed layer takes no write, so the view never changes, and an input read
//!   under a layer is one lookup in the layer's own entries (an open layer, under its lock) and
//!   one in its parent's view (no lock), whatever the depth of the chain;
//! - the memos of the queries it ran (re-runs, fixpoint iterations), in its own store; a memo of
//!   an ancestor or of the base that the layer verified gets a *stamp* in the layer, so siblings
//!   never see it. A base memo is valid in a layer by salsa's own revision test on the chain;
//! - for a tracked struct the layer re-created, an entry with the new fields, per-field revisions,
//!   durability and its own `updated_at` (the layer's revision); the struct keeps its id. A stale
//!   struct is tombstoned in the layer, never deleted from the base;
//! - its own claims, dependency graph and cycle-head transfers, so that two layers never block or
//!   join each other;
//! - the table slots it owns: a tracked struct it created with a new identity and a value it
//!   interned. They are given back when the layer drops and taken again by later layers with a
//!   bumped generation, or the same id for an immortal interned value (module `owned`). An interned value a layer inserted is seen only by that
//!   layer's chain: a lookup matches a value of the base or of a layer of the chain, so siblings
//!   that intern the same content get an id each (the linear history of each chain, rather than
//!   one id for a content in every layer).
//!
//! A handle without a layer behaves as upstream salsa.
//!
//! Invariants enforced here, each a panic in release builds too:
//!
//! - a handle with a layer never takes `zalsa_mut` (a setter, `synthetic_write`, LRU eviction,
//!   `memory_usage`, ...): it panics instead of waiting for other handles;
//! - a base is frozen only through its only handle, so no write of the base is in flight;
//! - a frozen base takes no write except the shared writes of [`SharedWrites`]: no memo insert,
//!   no `verified_at`, `verified_final`, accumulated-inputs or cycle-iteration store on a base
//!   memo, no tracked-struct update or deletion in place, no interned durability, revision-queue
//!   or LRU write, no slot reuse, from any handle;
//! - a sealed layer takes no write (an input, a memo, a stamp, a tracked-struct entry or a
//!   tombstone), and only a sealed layer gets a child;
//! - a layer's cancellation is final: no handle's `attach` scope resets the shared token;
//! - a layer of another base, or of an earlier freeze of this one, cannot be set on a handle;
//! - a layered input write is refused for a field whose durability is above `LOW`.
//!
//! LRU (E8): layers record no LRU use, so a layer evicts nothing and the base's LRU keeps the
//! order of the base's own reads. The base evicts nothing while frozen, nor at the new revision
//! of a [thaw](try_thaw): a value evicted there would be recomputed, and never written back, by
//! every layer of a later freeze. Its values therefore grow only through its own queries; the
//! base's own next new revision (a setter, `synthetic_write`) evicts as upstream does.

mod owned;
mod store;
mod view;

use std::any::Any;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

pub(crate) use self::owned::LayerFreeList;
use self::owned::{OwnedSlots, SlotCounters};
use self::store::Sharded;
pub use self::view::ZalsaView;
use crate::input::Configuration as InputConfiguration;
use crate::runtime::DependencyGraph;
use crate::storage::HasStorage;
use crate::sync::{Arc, Mutex, MutexGuard, OnceLock};
use crate::table::memo::Memo as ErasedMemoTrait;
use crate::zalsa::{IngredientIndex, Zalsa};
use crate::zalsa_local::CancellationToken;
use crate::{Database, Durability, Id, Revision};

/// The process-wide layer revision counter: the revision of the newest layer handed out, or the
/// highest base revision a [`freeze`] raised it to. Every new layer takes the next value, so a
/// layer's revision exceeds its parent's and every revision any frozen base had at its freeze.
static LAYER_REVISION: AtomicUsize = AtomicUsize::new(0);

/// Source of the identities that tie a layer to one freeze of its base (never 0).
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
    /// The identity of the current freeze of this base, new at every [`freeze`] (0 before the
    /// first). A layer carries the epoch it was made under, so a layer that outlived a thaw is
    /// refused after a refreeze: its stamps are keyed by the addresses of memos the thawed base
    /// may have freed and reused, and its memos were verified against the base before the thaw.
    epoch: AtomicU64,
    frozen: AtomicBool,
    slot_allocations: AtomicUsize,
    interned_inserts: AtomicUsize,
    last_interned_at_stores: AtomicUsize,
    sync_claims: AtomicUsize,
    skipped_durability_stores: AtomicUsize,
    skipped_revision_queue_records: AtomicUsize,
    skipped_lru_uses: AtomicUsize,
    refused_writes: AtomicUsize,
    /// The accounting of layer-owned slots, shared with every layer (a dropping layer counts its
    /// frees without a reference to this state).
    slots: Arc<SlotCounters>,
    /// The `allocated` count of every table page at the current freeze: the slots the base
    /// fingerprint reads. A slot a layer may take again is never among them (I2).
    frozen_allocated: Mutex<Vec<usize>>,
}

impl ForkState {
    pub(crate) fn new() -> Self {
        ForkState {
            epoch: AtomicU64::new(0),
            frozen: AtomicBool::new(false),
            slot_allocations: AtomicUsize::new(0),
            interned_inserts: AtomicUsize::new(0),
            last_interned_at_stores: AtomicUsize::new(0),
            sync_claims: AtomicUsize::new(0),
            skipped_durability_stores: AtomicUsize::new(0),
            skipped_revision_queue_records: AtomicUsize::new(0),
            skipped_lru_uses: AtomicUsize::new(0),
            refused_writes: AtomicUsize::new(0),
            slots: Arc::new(SlotCounters::default()),
            frozen_allocated: Mutex::new(Vec::new()),
        }
    }

    /// Counts an input created under a layer: a slot kept for the life of the database.
    pub(crate) fn record_created_input(&self) {
        SlotCounters::add(&self.slots.created_inputs, 1);
    }

    #[inline]
    pub(crate) fn is_frozen(&self) -> bool {
        self.frozen.load(Ordering::Acquire)
    }

    /// The identity of the current freeze (see the field).
    #[inline]
    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Panics if the base is frozen: `what` names the write that reached it. The refusal is
    /// counted first, so a caller that catches the panic (rust-analyzer catches panics of a
    /// query) cannot hide it from [`refused_writes`].
    #[inline]
    #[track_caller]
    pub(crate) fn assert_not_frozen(&self, what: &str) {
        if self.is_frozen() {
            self.refused_writes.fetch_add(1, Ordering::Relaxed);
            frozen_base_write(what);
        }
    }

    /// Counts one shared write of the kind `kind` made under a layer.
    #[inline]
    pub(crate) fn record_shared_write(&self, kind: SharedWriteKind) {
        let counter = match kind {
            SharedWriteKind::SlotAllocation => &self.slot_allocations,
            SharedWriteKind::InternedInsert => &self.interned_inserts,
            SharedWriteKind::LastInternedAt => &self.last_interned_at_stores,
            SharedWriteKind::SyncClaim => &self.sync_claims,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts one base write that a layer reached and did not make (it is not on the list).
    #[inline]
    pub(crate) fn record_skipped_write(&self, kind: SkippedWriteKind) {
        let counter = match kind {
            SkippedWriteKind::InternedDurability => &self.skipped_durability_stores,
            SkippedWriteKind::RevisionQueue => &self.skipped_revision_queue_records,
            SkippedWriteKind::LruUse => &self.skipped_lru_uses,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a refusal that panics outside [`ForkState::assert_not_frozen`].
    pub(crate) fn record_refusal(&self) {
        self.refused_writes.fetch_add(1, Ordering::Relaxed);
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
    /// A table slot allocated under a layer: an input created under a layer, a new interned
    /// value, or a tracked struct the layer created anew (a new identity, or an identity-field
    /// change), which only the layer reaches.
    SlotAllocation,
    /// A new interned value inserted into its shard's key map.
    InternedInsert,
    /// `last_interned_at` raised on an existing interned value.
    LastInternedAt,
    /// A claim in a function's sync table, keyed by the layer.
    SyncClaim,
}

/// Base writes a layer reaches and does not make (D11, E2: "not done").
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum SkippedWriteKind {
    /// An interned value's durability raised by a layer's query.
    InternedDurability,
    /// A layer revision recorded in an interned ingredient's revision queue.
    RevisionQueue,
    /// An LRU use recorded for a memo read under a layer.
    LruUse,
}

/// The isolation counters of a base: how many shared writes of each listed kind layers made
/// to it since it was created. A base write of any other kind under a frozen base panics, so it
/// has no counter (only [`refused_writes`] counts the refusals).
// Not `non_exhaustive`: a test destructures it exhaustively, so the list of kinds is pinned
// (E2, `shared_writes_under_a_layer_are_the_listed_ones`).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct SharedWrites {
    /// Table slots allocated under a layer (an input, an interned value or a tracked struct
    /// created through a layered handle).
    pub slot_allocations: usize,
    /// New interned values inserted into a key map under a layer.
    pub interned_inserts: usize,
    /// `last_interned_at` stores on existing interned values under a layer.
    pub last_interned_at_stores: usize,
    /// Claims taken under a layer in a function's sync table (keyed by the layer).
    pub sync_claims: usize,
    /// Slots a dropped layer gave back and a later layer took again, with a bumped generation (a
    /// tracked struct with a new identity, an interned value).
    pub slot_reuses: usize,
    /// Slots dropped layers gave back to their ingredient's layer free list.
    pub slot_frees: usize,
}

/// The table slots of a base that layers own or kept, now (D11, I2).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LayerSlots {
    /// Slots live layers own: allocated or taken again under a layer and not yet given back by its
    /// drop. 0 once every layer has dropped.
    pub owned: usize,
    /// Slots given back that will never be taken again: a generation that cannot be bumped, or a
    /// slot given back after a thaw by a layer of an earlier freeze.
    pub leaked: usize,
    /// Inputs created under a layer. They are not owned and never given back: rust-analyzer's
    /// `Files` map, which every handle of the base shares, keeps their ids, and a later creation
    /// of the same file in any layer sets the text of that input. So they grow with the distinct
    /// files created, not with the layers.
    pub created_inputs: usize,
}

/// The base writes layers reached and did not make, by kind (D11, E2 "not done").
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SkippedWrites {
    /// Interned durability raises.
    pub durability_stores: usize,
    /// Interned revision-queue records.
    pub revision_queue_records: usize,
    /// LRU uses of memos.
    pub lru_uses: usize,
}

/// One input struct's field values as a layer sees them: the fields, and per field its revision
/// and durability. Kept for the life of the layer; a later write in the same layer adds a new
/// entry and leaves this one in place, so a reference handed out from it stays valid.
pub(crate) struct InputEntry<C: InputConfiguration> {
    pub(crate) fields: C::Fields,
    pub(crate) revisions: Box<[Revision]>,
    pub(crate) durabilities: Box<[Durability]>,
}

/// A type-erased [`InputEntry`], shared between the layer that set it and the flattened views of
/// that layer and its descendants.
type SharedInputEntry = Arc<dyn Any + Send + Sync>;

/// The flattened input view of a sealed layer: `(input struct ingredient, id)` to the newest
/// entry the layer or one of its ancestors set. Built once, at the seal, and never changed.
type FlatInputs = FxHashMap<(IngredientIndex, Id), SharedInputEntry>;

/// What a layer holds for a tracked struct id: an entry (an index into `values`) or a tombstone.
#[derive(Copy, Clone, Debug)]
enum TrackedSlot {
    Entry(usize),
    Tombstone,
}

/// A memo found on a layer's chain.
pub(crate) enum ChainMemo<'a> {
    /// A layer of the chain holds a memo for the key.
    Found(&'a dyn ErasedMemoTrait),
    /// A layer of the chain tombstoned the id before any layer above it computed the key: the
    /// memos of older layers and of the base are not visible.
    Hidden,
    /// No layer holds a memo for the key nor tombstoned the id: look in the base.
    NotInLayers,
}

/// A tracked struct as a layer's chain sees it.
pub(crate) enum ChainTracked<'a> {
    /// A layer of the chain holds an entry for the id; `own` when it is this layer.
    Entry {
        value: &'a (dyn Any + Send + Sync),
        own: bool,
    },
    /// A layer of the chain tombstoned the id.
    Tombstone,
    /// No layer holds the id: the base slot is current.
    None,
}

/// What a layer did, counted by the layer (for the gates of D11's stage 2).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LayerStats {
    /// Deep verifications of a memo run under this layer.
    pub deep_verifies: usize,
    /// Memos inserted into this layer (executions and fixpoint iterations).
    pub memos: usize,
    /// Stamps this layer put on memos of its ancestors or of the base.
    pub stamps: usize,
    /// Tracked-struct entries this layer holds.
    pub tracked_entries: usize,
    /// Tracked structs this layer tombstoned.
    pub tombstones: usize,
}

/// One write on top of a parent: a revision, the parent chain, a sealed flag, a cancellation
/// token, the input values it set, the memos it computed, its stamps, its tracked-struct entries
/// and its dependency graph.
pub struct Layer {
    /// The epoch of the freeze of the base this layer was made under (`ForkState::epoch`).
    base: u64,
    revision: Revision,
    parent: Option<Arc<Layer>>,
    sealed: AtomicBool,
    written: AtomicBool,
    /// Whether a query wrote into the layer (a memo, stamp, entry or tombstone).
    queried: AtomicBool,
    /// Held by an input write for its whole read-modify-insert and by [`Layer::seal`]: a write
    /// and a seal do not interleave, and two writes to one input in one layer lose neither.
    write: Mutex<()>,
    token: CancellationToken,
    /// `(input struct ingredient, id)` to the index of the newest entry in `input_values`.
    inputs: Mutex<FxHashMap<(IngredientIndex, Id), usize>>,
    /// Append-only: an input entry stays until the layer is dropped (and as long as a flattened
    /// view of a descendant holds it).
    input_values: boxcar::Vec<SharedInputEntry>,
    /// The flattened view of the chain's inputs, set once by [`Layer::seal`] (before the sealed
    /// flag) and never changed: a set view means the layer takes no input write.
    flat_inputs: OnceLock<Arc<FlatInputs>>,
    /// Append-only: an entry stays until the layer is dropped. Tracked-struct entries.
    values: boxcar::Vec<Box<dyn Any + Send + Sync>>,
    /// `(function ingredient, id)` to the index of the newest memo in `memo_values`.
    memos: Sharded<(IngredientIndex, Id), usize>,
    /// Append-only: a memo the layer replaced (a later fixpoint iteration) stays until the layer
    /// is dropped, so a reference handed out from it stays valid.
    memo_values: boxcar::Vec<Box<dyn ErasedMemoTrait>>,
    /// The function ingredients with a memo in this layer, per id (for stale-output removal).
    memo_functions: Mutex<FxHashMap<Id, SmallVec<[IngredientIndex; 2]>>>,
    /// The address of a memo of an ancestor or of the base that this layer verified, to the
    /// layer's revision. Ancestor and base memos stay at their address for the life of the layer
    /// under the freeze it was made in (see `ForkState::epoch`).
    stamps: Sharded<usize, Revision>,
    /// Tracked-struct entries and tombstones, by id.
    tracked: Sharded<Id, TrackedSlot>,
    /// Blocked threads and cycle-head transfers of this layer's queries (E5).
    dependency_graph: Mutex<DependencyGraph>,
    deep_verifies: AtomicUsize,
    memo_count: AtomicUsize,
    stamp_count: AtomicUsize,
    tracked_count: AtomicUsize,
    tombstone_count: AtomicUsize,
    /// The accounting of layer-owned slots of the base (shared with `ForkState`).
    slot_counters: Arc<SlotCounters>,
    /// The table slots this layer owns, given back when it drops.
    owned: Mutex<OwnedSlots>,
}

/// The locks of a layer are held only for one map lookup or insert, which runs no user code and
/// cannot panic halfway, and its vectors are append-only. The `write` lock guards no data: a
/// setter that panics under it unwinds before the insert, so the layer keeps its old entry. The
/// dependency graph is the base runtime's kind and is left consistent by the same code. A panic
/// elsewhere leaves no torn state, as upstream argues for [`Zalsa`].
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

/// A dropping layer gives back the slots it owns: nothing reaches them any more (its handles and
/// children, which hold it, are gone), so a later layer may take them again.
impl Drop for Layer {
    fn drop(&mut self) {
        let given = self.owned.lock().give_back(self.base);
        if given > 0 {
            SlotCounters::add(&self.slot_counters.frees, given);
            self.slot_counters
                .owned
                .fetch_sub(given, Ordering::Relaxed);
        }
    }
}

impl Layer {
    fn new(base: u64, slot_counters: Arc<SlotCounters>, parent: Option<Arc<Layer>>) -> Arc<Layer> {
        Arc::new(Layer {
            slot_counters,
            owned: Mutex::new(OwnedSlots::default()),
            base,
            revision: next_layer_revision(),
            parent,
            sealed: AtomicBool::new(false),
            written: AtomicBool::new(false),
            queried: AtomicBool::new(false),
            write: Mutex::new(()),
            token: CancellationToken::default(),
            inputs: Mutex::new(FxHashMap::default()),
            input_values: boxcar::Vec::new(),
            flat_inputs: OnceLock::new(),
            values: boxcar::Vec::new(),
            memos: Sharded::new(),
            memo_values: boxcar::Vec::new(),
            memo_functions: Mutex::new(FxHashMap::default()),
            stamps: Sharded::new(),
            tracked: Sharded::new(),
            dependency_graph: Mutex::new(DependencyGraph::default()),
            deep_verifies: AtomicUsize::new(0),
            memo_count: AtomicUsize::new(0),
            stamp_count: AtomicUsize::new(0),
            tracked_count: AtomicUsize::new(0),
            tombstone_count: AtomicUsize::new(0),
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
    /// write either completes before the seal or is refused after it, and builds the layer's
    /// flattened input view from its own entries and its parent's view.
    pub fn seal(&self) {
        let _write = self.write.lock();
        self.flat_inputs.get_or_init(|| self.flatten_inputs());
        self.sealed.store(true, Ordering::Release);
    }

    /// The flattened input view of this layer: the parent's view (empty for a root layer) with
    /// this layer's own entries over it. A layer that set no input shares its parent's view; one
    /// that did copies the parent's map (one entry per input set on the chain, each an `Arc`).
    /// Runs under the write lock, so the own entries are complete.
    fn flatten_inputs(&self) -> Arc<FlatInputs> {
        let parent = self.parent.as_ref().map(|p| p.sealed_inputs());
        let own = self.inputs.lock();
        if own.is_empty() {
            return parent.cloned().unwrap_or_default();
        }
        let mut flat = parent.map(|p| FlatInputs::clone(p)).unwrap_or_default();
        flat.reserve(own.len());
        for (&key, &index) in own.iter() {
            let entry = self
                .input_values
                .get(index)
                .expect("ryo-v2 fork: an input entry index names a pushed value");
            flat.insert(key, Arc::clone(entry));
        }
        Arc::new(flat)
    }

    /// The flattened input view of this sealed layer.
    #[inline]
    fn sealed_inputs(&self) -> &Arc<FlatInputs> {
        self.flat_inputs
            .get()
            .expect("ryo-v2 fork: a sealed layer has its flattened input view")
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

    /// What this layer did so far.
    pub fn stats(&self) -> LayerStats {
        LayerStats {
            deep_verifies: self.deep_verifies.load(Ordering::Relaxed),
            memos: self.memo_count.load(Ordering::Relaxed),
            stamps: self.stamp_count.load(Ordering::Relaxed),
            tracked_entries: self.tracked_count.load(Ordering::Relaxed),
            tombstones: self.tombstone_count.load(Ordering::Relaxed),
        }
    }

    /// The layers of the chain, this one first, then its ancestors.
    pub(crate) fn chain(&self) -> impl Iterator<Item = &Layer> {
        std::iter::successors(Some(self), |l| l.parent.as_deref())
    }

    /// The key of this layer's claims: its revision, unique in the process and never 0 (the key
    /// of a base handle's claims).
    #[inline]
    pub(crate) fn key(&self) -> usize {
        self.revision.as_usize()
    }

    /// The identity of the freeze this layer was made under (`ForkState::epoch`).
    #[inline]
    pub(crate) fn epoch(&self) -> u64 {
        self.base
    }

    /// Whether a value owned by `owner` (a layer's [`Layer::key`], 0 for the base) is seen by this
    /// layer's chain.
    #[inline]
    pub(crate) fn sees_owner(&self, owner: usize) -> bool {
        owner == 0 || self.chain().any(|l| l.key() == owner)
    }

    /// Records slot `id` of `shard` of `list`'s ingredient as owned by this layer: given back
    /// when it drops. `reused` when it was taken again from the free list.
    pub(crate) fn own_slot(&self, list: &Arc<LayerFreeList>, shard: u16, id: Id, reused: bool) {
        self.owned.lock().insert(list, shard, id);
        SlotCounters::add(&self.slot_counters.owned, 1);
        if reused {
            SlotCounters::add(&self.slot_counters.reuses, 1);
        }
    }

    /// Counts a slot of a free list that will never be taken again.
    pub(crate) fn record_leaked_slot(&self) {
        SlotCounters::add(&self.slot_counters.leaked, 1);
    }

    /// The revision of the newest layer of the chain that has writes, if any.
    pub(crate) fn last_written(&self) -> Option<Revision> {
        self.chain().find(|l| l.has_writes()).map(|l| l.revision)
    }

    /// Whether an ancestor's token (not this layer's own) was cancelled.
    pub(crate) fn ancestor_cancelled(&self) -> bool {
        self.chain().skip(1).any(|l| l.token.is_cancelled())
    }

    /// The dependency graph of this layer's blocked threads and transfers.
    #[inline]
    pub(crate) fn dependency_graph(&self) -> &Mutex<DependencyGraph> {
        &self.dependency_graph
    }

    /// The newest values of input `id` of ingredient `ingredient` on the chain: a sealed layer
    /// looks in its own flattened view; an open one in its own entries, then in its parent's
    /// view. At most two lookups, whatever the depth; `None` means the base slot is current.
    pub(crate) fn input<C: InputConfiguration>(
        &self,
        ingredient: IngredientIndex,
        id: Id,
    ) -> Option<&InputEntry<C>> {
        let key = (ingredient, id);
        let entry: &(dyn Any + Send + Sync) = match self.flat_inputs.get() {
            Some(flat) => &**flat.get(&key)?,
            None => {
                let index = self.inputs.lock().get(&key).copied();
                match index {
                    Some(index) => &**self
                        .input_values
                        .get(index)
                        .expect("ryo-v2 fork: an input entry index names a pushed value"),
                    None => &**self.parent.as_ref()?.sealed_inputs().get(&key)?,
                }
            }
        };
        Some(
            entry
                .downcast_ref::<InputEntry<C>>()
                .expect("ryo-v2 fork: an input entry has its ingredient's type"),
        )
    }

    /// Takes the layer's write lock for an input write into it, after refusing a sealed layer.
    /// The write must not write into this layer again while it holds the guard (the lock is not
    /// reentrant).
    #[track_caller]
    pub(crate) fn begin_write(&self, what: &str) -> MutexGuard<'_, ()> {
        let write = self.write.lock();
        self.assert_open(what);
        // A layer has one revision: a memo verified or computed in it is verified at that
        // revision, so a later write in the same layer would leave it looking current.
        if self.queried.load(Ordering::Acquire) {
            panic!(
                "ryo-v2 fork: {what} into a layer after a query ran in it (a layer's writes come \
                 before its queries; write into a child instead)"
            );
        }
        write
    }

    /// Records that a query wrote into the layer (a memo, a stamp, a tracked-struct entry or a
    /// tombstone): no input write may follow.
    #[inline]
    fn mark_queried(&self) {
        if !self.queried.load(Ordering::Relaxed) {
            self.queried.store(true, Ordering::Release);
        }
    }

    /// Stores new values of input `id` in this layer. Panics on a sealed layer.
    pub(crate) fn insert_input<C: InputConfiguration>(
        &self,
        ingredient: IngredientIndex,
        id: Id,
        entry: InputEntry<C>,
    ) {
        self.assert_open("an input write");
        let index = self.input_values.push(Arc::new(entry));
        self.inputs.lock().insert((ingredient, id), index);
        self.written.store(true, Ordering::Release);
    }

    /// The memo of `function` for `id` on the chain: the newest layer that holds one, unless a
    /// layer below it and above the memo tombstoned `id`.
    pub(crate) fn find_memo(&self, function: IngredientIndex, id: Id) -> ChainMemo<'_> {
        for layer in self.chain() {
            if let Some(index) = layer.memos.get(&(function, id)) {
                let memo = layer
                    .memo_values
                    .get(index)
                    .expect("ryo-v2 fork: a memo index names a pushed memo");
                return ChainMemo::Found(&**memo);
            }
            if let Some(TrackedSlot::Tombstone) = layer.tracked.get(&id) {
                return ChainMemo::Hidden;
            }
        }
        ChainMemo::NotInLayers
    }

    /// Stores `memo` as this layer's memo of `function` for `id`, replacing an older one of the
    /// layer (which stays allocated until the layer is dropped). Panics on a sealed layer (E9).
    pub(crate) fn insert_memo(
        &self,
        function: IngredientIndex,
        id: Id,
        memo: Box<dyn ErasedMemoTrait>,
    ) -> &dyn ErasedMemoTrait {
        self.assert_open("a memo insert");
        self.mark_queried();
        let index = self.memo_values.push(memo);
        let previous = self.memos.insert((function, id), index);
        if previous.is_none() {
            self.memo_functions
                .lock()
                .entry(id)
                .or_default()
                .push(function);
        }
        self.memo_count.fetch_add(1, Ordering::Relaxed);
        &**self
            .memo_values
            .get(index)
            .expect("ryo-v2 fork: the memo was just pushed")
    }

    /// The function ingredients with a memo for `id` visible on the chain (up to the newest
    /// tombstone of `id`), and whether the base's memos of `id` are visible too.
    pub(crate) fn memo_functions(&self, id: Id) -> (SmallVec<[IngredientIndex; 4]>, bool) {
        let mut out: SmallVec<[IngredientIndex; 4]> = SmallVec::new();
        for layer in self.chain() {
            if let Some(functions) = layer.memo_functions.lock().get(&id) {
                for f in functions {
                    if !out.contains(f) {
                        out.push(*f);
                    }
                }
            }
            if let Some(TrackedSlot::Tombstone) = layer.tracked.get(&id) {
                return (out, false);
            }
        }
        (out, true)
    }

    /// The newest stamp on the chain of the memo whose header is at `address`.
    #[inline]
    pub(crate) fn stamp_of(&self, address: usize) -> Option<Revision> {
        self.chain().find_map(|layer| layer.stamps.get(&address))
    }

    /// Stamps the memo whose header is at `address` (a memo of an ancestor or of the base) as
    /// verified at this layer's revision. Panics on a sealed layer.
    pub(crate) fn stamp(&self, address: usize) {
        self.assert_open("a memo stamp");
        self.mark_queried();
        if self.stamps.insert(address, self.revision).is_none() {
            self.stamp_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Counts a deep verification run under this layer.
    #[inline]
    pub(crate) fn count_deep_verify(&self) {
        self.deep_verifies.fetch_add(1, Ordering::Relaxed);
    }

    /// The tracked struct `id` as the chain sees it.
    pub(crate) fn find_tracked(&self, id: Id) -> ChainTracked<'_> {
        for layer in self.chain() {
            match layer.tracked.get(&id) {
                Some(TrackedSlot::Entry(index)) => {
                    let value = layer
                        .values
                        .get(index)
                        .expect("ryo-v2 fork: a tracked entry index names a pushed value");
                    return ChainTracked::Entry {
                        value: &**value,
                        own: std::ptr::eq(layer, self),
                    };
                }
                Some(TrackedSlot::Tombstone) => return ChainTracked::Tombstone,
                None => {}
            }
        }
        ChainTracked::None
    }

    /// Stores `entry` as this layer's entry of the tracked struct `id`. Panics on a sealed layer.
    pub(crate) fn insert_tracked(
        &self,
        id: Id,
        entry: Box<dyn Any + Send + Sync>,
    ) -> &(dyn Any + Send + Sync) {
        self.assert_open("a tracked-struct entry");
        self.mark_queried();
        let index = self.values.push(entry);
        self.tracked.insert(id, TrackedSlot::Entry(index));
        self.tracked_count.fetch_add(1, Ordering::Relaxed);
        &**self
            .values
            .get(index)
            .expect("ryo-v2 fork: the entry was just pushed")
    }

    /// Tombstones the tracked struct `id` in this layer. Panics on a sealed layer.
    pub(crate) fn tombstone(&self, id: Id) {
        self.assert_open("a tracked-struct tombstone");
        self.mark_queried();
        self.tracked.insert(id, TrackedSlot::Tombstone);
        self.tombstone_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Panics if the layer is sealed: `what` names the write that reached it.
    #[track_caller]
    pub(crate) fn assert_open(&self, what: &str) {
        if self.is_sealed() {
            panic!("ryo-v2 fork: write to a sealed layer ({what})");
        }
    }
}

/// Panics: `what` is not routed through a layer, and a layered handle reached it.
#[cold]
#[inline(never)]
#[track_caller]
pub(crate) fn not_yet_supported(what: &str) -> ! {
    panic!("ryo-v2 fork: not supported under a layer: {what}")
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
    // A new identity for this freeze: layers of an earlier freeze are refused by `set_layer`.
    state
        .epoch
        .store(BASE_ID.fetch_add(1, Ordering::Relaxed), Ordering::Release);
    // The slots the base fingerprint reads: those present now. No layer exists yet, so none of
    // them is layer-owned under this freeze.
    *state.frozen_allocated.lock() = view.table().fork_allocated();
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
    Layer::new(view.fork().epoch(), Arc::clone(&view.fork().slots), None)
}

/// A new open layer on `parent`.
///
/// Panics if `parent` is not sealed.
pub fn child_layer(parent: &Arc<Layer>) -> Arc<Layer> {
    assert!(
        parent.is_sealed(),
        "ryo-v2 fork: only a sealed layer gets a child"
    );
    Layer::new(
        parent.base,
        Arc::clone(&parent.slot_counters),
        Some(Arc::clone(parent)),
    )
}

/// Sets `layer` on the handle `db`: from here on its reads resolve through the layer's chain,
/// its writes go to the layer, and it uses the layer's cancellation token. Clones of the handle
/// keep the layer.
///
/// Panics if `db` already carries a layer, its base is not frozen, or `layer` descends from
/// another base or from an earlier freeze of this one (a layer that outlived a thaw).
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
        view.fork().epoch(),
        "ryo-v2 fork: the layer descends from another base, or from an earlier freeze of this one"
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
        interned_inserts: state.interned_inserts.load(Ordering::Relaxed),
        last_interned_at_stores: state.last_interned_at_stores.load(Ordering::Relaxed),
        sync_claims: state.sync_claims.load(Ordering::Relaxed),
        slot_reuses: SlotCounters::get(&state.slots.reuses),
        slot_frees: SlotCounters::get(&state.slots.frees),
    }
}

/// The table slots layers of the base `db` belongs to own or kept, now.
pub fn layer_slots<Db: HasStorage>(db: &Db) -> LayerSlots {
    let slots = &db.storage().fork_view().fork().slots;
    LayerSlots {
        owned: SlotCounters::get(&slots.owned),
        leaked: SlotCounters::get(&slots.leaked),
        created_inputs: SlotCounters::get(&slots.created_inputs),
    }
}

/// The base writes layers of the base `db` belongs to reached and did not make.
pub fn skipped_writes<Db: HasStorage>(db: &Db) -> SkippedWrites {
    let state = db.storage().fork_view().fork();
    SkippedWrites {
        durability_stores: state.skipped_durability_stores.load(Ordering::Relaxed),
        revision_queue_records: state.skipped_revision_queue_records.load(Ordering::Relaxed),
        lru_uses: state.skipped_lru_uses.load(Ordering::Relaxed),
    }
}

/// The writes to the frozen base `db` belongs to that were refused (each a panic, which a caller
/// may have caught). 0 is the isolation gate (I2).
pub fn refused_writes<Db: HasStorage>(db: &Db) -> usize {
    db.storage()
        .fork_view()
        .fork()
        .refused_writes
        .load(Ordering::Relaxed)
}

/// A summary of everything a base keeps that layers must not change (I2), per table slot: a hash
/// over the number of the slot's memos, every memo's `verified_at`, `changed_at` and durability,
/// an input's field revisions and durabilities, a tracked struct's `updated_at`, durability and
/// field revisions, and an interned value's durability. Only the slots present at the freeze are
/// read, less the interned values a layer of an earlier freeze owned (a layer may take those
/// again): a slot allocated or taken again under a layer is not base state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseFingerprint {
    /// `(page, slot, hash)` of every slot, in table order.
    pub(crate) slots: Vec<(usize, usize, u64)>,
    /// The memos of the base's tables.
    pub memos: usize,
}

impl BaseFingerprint {
    /// The number of slots.
    pub fn slots(&self) -> usize {
        self.slots.len()
    }

    /// Whether every slot of `self` is unchanged in `later`. Slots `later` has and `self` has
    /// not are not compared. `Err`
    /// names the first slot that differs or is missing.
    pub fn preserved_in(&self, later: &BaseFingerprint) -> Result<(), String> {
        let later: FxHashMap<(usize, usize), u64> = later
            .slots
            .iter()
            .map(|&(page, slot, hash)| ((page, slot), hash))
            .collect();
        for &(page, slot, hash) in &self.slots {
            match later.get(&(page, slot)) {
                Some(&h) if h == hash => {}
                Some(_) => return Err(format!("slot {slot} of page {page} changed")),
                None => return Err(format!("slot {slot} of page {page} is missing")),
            }
        }
        Ok(())
    }
}

/// The fingerprint of the base `db` belongs to. Reads every slot the base had at the freeze;
/// layers may run queries meanwhile (their writes to those slots are atomic stores; the slots
/// they allocate or take again are not read), but the result is a consistent summary only with
/// none running.
///
/// Panics if the base is not frozen: on a base that takes writes, a query on another handle
/// could write a slot (a tracked struct's durability, an input's revisions) during the read.
pub fn base_fingerprint<Db: HasStorage>(db: &Db) -> BaseFingerprint {
    let view = db.storage().fork_view();
    assert!(
        view.fork().is_frozen(),
        "ryo-v2 fork: a base fingerprint needs a frozen base"
    );
    let frozen = view.fork().frozen_allocated.lock().clone();
    view.table().fork_fingerprint(&frozen)
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
/// ([`Zalsa::advance_revision_past`], which evicts no LRU value: E8), and lifts the frozen flag.
/// Returns the new base revision.
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
    // What dropped layers gave back becomes the base's again: tracked slots go to the base's own
    // free list, and layer-interned values leave the key maps (the base's lookup has no owner
    // check), their slots staying on the layer free lists.
    zalsa.fork_thaw_ingredients();
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
    /// Moves the current revision to `max(current, r) + 1` and runs what a new revision runs on
    /// the ingredients ([`Zalsa::new_revision`]) except LRU eviction (E8), without touching the
    /// last-changed revision of any durability: no base input changed. Used at thaw with `r` the
    /// highest layer revision, so the base's next state shares its revision with no layer.
    pub fn advance_revision_past(&mut self, r: Revision) -> Revision {
        self.runtime_mut().raise_current_revision(r);
        self.fork_new_revision_at_thaw()
    }

    /// Runs [`Ingredient::fork_thaw`](crate::ingredient::Ingredient::fork_thaw) on every
    /// ingredient, with the base's table.
    pub(crate) fn fork_thaw_ingredients(&mut self) {
        let (ingredients, table) = self.ingredients_and_table_mut();
        for ingredient in ingredients {
            ingredient.fork_thaw(table);
        }
    }
}
