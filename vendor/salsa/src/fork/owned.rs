// ryo-v2 fork: the table slots a layer owns and how they are given back when it drops (D11). Not upstream salsa code.
//! The table slots a [`Layer`](super::Layer) owns: a tracked struct it created with a new
//! identity, and a value it interned. Only the layer's chain reaches such a slot (a tracked struct
//! through the layer's own memos, an interned value through the chain-scoped lookup), and every
//! descendant holds its parent, so when the layer drops nothing reaches the slot any more. Its id
//! is then pushed onto the free list of its ingredient, shared by `Arc` between the ingredient and
//! the layers (a layer has no reference to its base's table), and a later layer takes the slot
//! again with a bumped generation, as salsa's own free list does (an immortal interned value
//! keeps its id: upstream never moves its generation, and rust-analyzer relies on that): the old
//! fields stay in the slot until it is taken.
//!
//! Created inputs are not owned: rust-analyzer's `Files` map, outside salsa and shared by every
//! handle of the base, keeps their ids (see [`LayerSlots::created_inputs`](super::LayerSlots)).

use std::sync::atomic::{AtomicUsize, Ordering};

use rustc_hash::FxHashMap;

use crate::Id;
use crate::sync::{Arc, Mutex};

/// A slot given back by a dropped layer: its id (the generation it had) and the freeze it was
/// owned under (`ForkState::epoch`).
#[derive(Copy, Clone, Debug)]
pub(crate) struct FreeSlot {
    pub(crate) epoch: u64,
    pub(crate) id: Id,
}

/// The slots of one ingredient that dropped layers gave back, per shard of the ingredient (one
/// for a tracked struct, the key-map shards for an interned value, whose slots never change
/// shard).
pub(crate) struct LayerFreeList {
    shards: Box<[Mutex<Vec<FreeSlot>>]>,
}

impl LayerFreeList {
    pub(crate) fn new(shards: usize) -> Arc<Self> {
        Arc::new(LayerFreeList {
            shards: (0..shards.max(1)).map(|_| Mutex::new(Vec::new())).collect(),
        })
    }

    fn push(&self, shard: u16, slots: impl Iterator<Item = FreeSlot>) {
        self.shards[shard as usize].lock().extend(slots);
    }

    /// The most recently given back slot of `shard`.
    pub(crate) fn pop(&self, shard: usize) -> Option<FreeSlot> {
        self.shards[shard].lock().pop()
    }

    /// Every slot given back so far, every shard.
    pub(crate) fn take_all(&self) -> Vec<FreeSlot> {
        let mut out = Vec::new();
        for shard in self.shards.iter() {
            out.append(&mut shard.lock());
        }
        out
    }
}

/// The accounting of layer-owned slots of one base, shared by the base and every layer of it.
#[derive(Default)]
pub(crate) struct SlotCounters {
    /// Slots taken again from a free list under a layer.
    pub(crate) reuses: AtomicUsize,
    /// Slots given back by a dropped layer.
    pub(crate) frees: AtomicUsize,
    /// Slots layers own now (allocated or taken again, not yet given back).
    pub(crate) owned: AtomicUsize,
    /// Slots never taken again: a generation that cannot be bumped, or a slot given back under an
    /// earlier freeze than the layer that found it.
    pub(crate) leaked: AtomicUsize,
    /// Inputs created under a layer (kept; not owned).
    pub(crate) created_inputs: AtomicUsize,
}

impl SlotCounters {
    #[inline]
    pub(crate) fn add(counter: &AtomicUsize, n: usize) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn get(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::Relaxed)
    }
}

/// The slots of one free list's ingredient a layer owns: `(shard, id)`.
type OwnedOf = (Arc<LayerFreeList>, Vec<(u16, Id)>);

/// The slots one layer owns, by free list (keyed by the list's address).
#[derive(Default)]
pub(crate) struct OwnedSlots {
    by_list: FxHashMap<usize, OwnedOf>,
}

impl OwnedSlots {
    pub(crate) fn insert(&mut self, list: &Arc<LayerFreeList>, shard: u16, id: Id) {
        let key = Arc::as_ptr(list).addr();
        self.by_list
            .entry(key)
            .or_insert_with(|| (Arc::clone(list), Vec::new()))
            .1
            .push((shard, id));
    }

    /// Gives every slot back to its free list under `epoch`; returns how many.
    pub(crate) fn give_back(&mut self, epoch: u64) -> usize {
        let mut n = 0;
        for (_, (list, slots)) in self.by_list.drain() {
            n += slots.len();
            let mut by_shard: FxHashMap<u16, Vec<FreeSlot>> = FxHashMap::default();
            for (shard, id) in slots {
                by_shard
                    .entry(shard)
                    .or_default()
                    .push(FreeSlot { epoch, id });
            }
            for (shard, slots) in by_shard {
                list.push(shard, slots.into_iter());
            }
        }
        n
    }
}
