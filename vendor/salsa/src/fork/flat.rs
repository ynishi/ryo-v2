// ryo-v2 fork: the chain view of a sealed layer (D11, the chain views). Not upstream salsa code.
//! The flattened memo, tombstone and tracked-struct view of a sealed layer's chain.
//!
//! A layer stores one delta: the memos it computed, the tracked structs it re-created and the ones
//! it tombstoned. A read walks the deltas of the chain, newest first. A sealed layer takes no
//! insert and no query is in flight on it (E9: [`Layer::seal`] needs no handle to carry it), so
//! what the walk resolves for a sealed layer is a fixed value. [`FlatChain`] is that value, built
//! once per sealed layer from its parent's view and its own delta; a lookup is then one probe of
//! the view instead of two per layer, whatever the depth.
//!
//! The view holds references, never memos: a [`Ref`] names a layer of the chain by its depth and
//! an entry of that layer's append-only store by its index, so a found memo is borrowed from the
//! layer that holds it, which the reading layer keeps alive through its ancestors.
//!
//! **Equal to the walk.** Write `W(L, k)` for the walk on `k = (function, id)`: an own memo of
//! `L` is found; else an own tombstone of `id` hides; else `W(parent)`, and nothing at the root.
//! The view of `L` is its parent's with the memos of the ids `L` tombstoned removed, those ids
//! added to `hidden`, and then `L`'s own memos inserted. By induction on the depth: an own memo of
//! `L` is in the view (inserted last, so a memo and a tombstone in one layer resolve to the memo,
//! as in the walk); a key of an id `L` tombstoned that is not an own memo is gone and its id is
//! hidden; any other key resolves as in the parent's view. Tracked entries are the parent's map
//! overwritten by the own slots, the newest slot per id, which is what the walk returns.

use std::mem::size_of;

use rustc_hash::{FxHashMap, FxHashSet};

use super::{Layer, TrackedSlot};
use crate::sync::Arc;
use crate::zalsa::IngredientIndex;
use crate::Id;

/// An entry of a layer of the chain: the layer by its depth, the entry by its index in that
/// layer's store; for a tracked id, `index == TOMBSTONE` is a tombstone.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) struct Ref {
    depth: u32,
    index: u32,
}

/// The index of a tombstone [`Ref`].
const TOMBSTONE: u32 = u32::MAX;

// The view's entries stay compact (gate M2-view): a reference is 8 bytes, a memo entry 20 (a
// 12-byte key) and a tracked entry 16.
const _: () = assert!(size_of::<Ref>() == 8);
const _: () = assert!(size_of::<((IngredientIndex, Id), Ref)>() == 20);
const _: () = assert!(size_of::<(Id, Ref)>() == 16);

impl Ref {
    fn entry(depth: usize, index: usize) -> Ref {
        Ref {
            depth: u32::try_from(depth).expect("ryo-v2 fork: a chain is shorter than 2^32 layers"),
            index: u32::try_from(index)
                .ok()
                .filter(|&index| index != TOMBSTONE)
                .expect("ryo-v2 fork: a layer holds fewer than 2^32 - 1 entries"),
        }
    }

    fn tombstone(depth: usize) -> Ref {
        Ref {
            depth: u32::try_from(depth).expect("ryo-v2 fork: a chain is shorter than 2^32 layers"),
            index: TOMBSTONE,
        }
    }

    /// The depth of the layer that holds the entry.
    #[inline]
    pub(super) fn depth(self) -> usize {
        self.depth as usize
    }

    /// The entry's index in its layer's store, `None` for a tombstone.
    #[inline]
    pub(super) fn index(self) -> Option<usize> {
        (self.index != TOMBSTONE).then_some(self.index as usize)
    }
}

/// What a view resolves for a memo key.
pub(super) enum ViewMemo {
    /// A layer of the chain holds the memo.
    Found(Ref),
    /// A layer of the chain tombstoned the id above every memo of the key.
    Hidden,
    /// No layer of the chain holds or hides it.
    Miss,
}

/// The resolved chain of one sealed layer: per memo key the newest memo no newer tombstone hides,
/// the ids a layer of the chain tombstoned, and per tracked id the newest entry or tombstone.
#[derive(Clone, Default)]
pub(super) struct FlatChain {
    memos: FxHashMap<(IngredientIndex, Id), Ref>,
    hidden: FxHashSet<Id>,
    tracked: FxHashMap<Id, Ref>,
}

impl FlatChain {
    /// The view of the sealed, quiescent `layer`, from `parent` (its parent's view; `None` for a
    /// root layer). A layer with no own memo, tracked entry or tombstone shares `parent`; any
    /// other copies it and applies its delta. Runs no user code.
    pub(super) fn build(layer: &Layer, parent: Option<&Arc<FlatChain>>) -> Arc<FlatChain> {
        let mut own_memos = Vec::new();
        layer
            .memos
            .for_each(|key, &index| own_memos.push((*key, index)));
        let mut own_tracked = Vec::new();
        layer
            .tracked
            .for_each(|&id, &slot| own_tracked.push((id, slot)));
        if own_memos.is_empty() && own_tracked.is_empty() {
            return parent.cloned().unwrap_or_default();
        }
        let mut flat = parent.map(|p| FlatChain::clone(p)).unwrap_or_default();
        let depth = layer.depth();
        let mut tombstoned: FxHashSet<Id> = FxHashSet::default();
        // No `reserve` by the own entries' count: most of them overwrite keys the parent's view
        // already holds (layers that run the same check compute mostly the same keys), and a
        // reserve for them all would double the buckets of a clone that is already near full.
        for (id, slot) in own_tracked {
            let at = match slot {
                TrackedSlot::Entry(index) => Ref::entry(depth, index),
                TrackedSlot::Tombstone => {
                    tombstoned.insert(id);
                    Ref::tombstone(depth)
                }
            };
            flat.tracked.insert(id, at);
        }
        if !tombstoned.is_empty() {
            flat.memos.retain(|(_, id), _| !tombstoned.contains(id));
            flat.hidden.extend(tombstoned);
        }
        for (key, index) in own_memos {
            flat.memos.insert(key, Ref::entry(depth, index));
        }
        Arc::new(flat)
    }

    /// The memo of `key` on the chain. Two probes at most (counted by the caller).
    #[inline]
    pub(super) fn memo(&self, key: &(IngredientIndex, Id)) -> (ViewMemo, usize) {
        if let Some(&at) = self.memos.get(key) {
            return (ViewMemo::Found(at), 1);
        }
        if self.hidden.contains(&key.1) {
            (ViewMemo::Hidden, 2)
        } else {
            (ViewMemo::Miss, 2)
        }
    }

    /// The newest tracked entry or tombstone of `id` on the chain.
    #[inline]
    pub(super) fn tracked(&self, id: Id) -> Option<Ref> {
        self.tracked.get(&id).copied()
    }

    /// The memo keys of the view.
    pub(super) fn memo_count(&self) -> usize {
        self.memos.len()
    }

    /// The tracked ids of the view (entries and tombstones).
    pub(super) fn tracked_count(&self) -> usize {
        self.tracked.len()
    }

    /// The hidden ids of the view.
    pub(super) fn hidden_count(&self) -> usize {
        self.hidden.len()
    }

    /// The heap bytes of the view's maps, from their capacity: per map its buckets (one more than
    /// the capacity below 8 buckets, 8/7 of it from 8), one entry and one control byte per
    /// bucket, and 16 trailing control bytes; 0 for a map that never allocated.
    pub(super) fn bytes(&self) -> usize {
        table_bytes(
            self.memos.capacity(),
            size_of::<((IngredientIndex, Id), Ref)>(),
        ) + table_bytes(self.hidden.capacity(), size_of::<Id>())
            + table_bytes(self.tracked.capacity(), size_of::<(Id, Ref)>())
    }
}

/// The bytes of a std (hashbrown) table of `capacity` and entries of `entry` bytes.
fn table_bytes(capacity: usize, entry: usize) -> usize {
    let buckets = match capacity {
        0 => return 0,
        c if c < 8 => c + 1,
        c => c / 7 * 8,
    };
    buckets * entry + buckets + 16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fork::owned::SlotCounters;
    use crate::fork::{ChainMemo, ChainTracked, ZalsaView};
    use crate::table::memo::Memo;
    use crate::DatabaseKeyIndex;
    use std::any::Any;

    /// A memo with a tag; its address identifies it.
    struct TestMemo(#[allow(dead_code)] u64);

    impl Memo for TestMemo {
        fn has_value(&self) -> bool {
            true
        }

        fn remove_outputs(&self, _zalsa: &ZalsaView, _executor: DatabaseKeyIndex) {}

        #[cfg(feature = "salsa_unstable")]
        fn memory_usage(&self) -> crate::database::MemoInfo {
            crate::database::MemoInfo {
                debug_name: "test",
                output: crate::database::SlotInfo {
                    debug_name: "test",
                    size_of_metadata: 0,
                    size_of_fields: 0,
                    heap_size_of_fields: None,
                    memos: Vec::new(),
                },
            }
        }
    }

    /// What a lookup resolved, comparable across the view and the walk: a found entry by its
    /// address.
    #[derive(Debug, PartialEq, Eq)]
    enum Seen {
        Found(usize),
        Hidden,
        Miss,
    }

    fn addr<T: ?Sized>(r: &T) -> usize {
        std::ptr::from_ref(r).cast::<()>() as usize
    }

    fn seen_memo(found: ChainMemo<'_>) -> Seen {
        match found {
            ChainMemo::Found(memo) => Seen::Found(addr(memo)),
            ChainMemo::Hidden => Seen::Hidden,
            ChainMemo::NotInLayers => Seen::Miss,
        }
    }

    fn seen_tracked(found: ChainTracked<'_>) -> (Seen, bool) {
        match found {
            ChainTracked::Entry { value, own } => (Seen::Found(addr(value)), own),
            ChainTracked::Tombstone => (Seen::Hidden, false),
            ChainTracked::None => (Seen::Miss, false),
        }
    }

    /// The walk the view replaces (the code before the chain views), as the oracle.
    fn walk_memo(layer: &Layer, function: IngredientIndex, id: Id) -> Seen {
        for l in layer.chain() {
            if let Some(index) = l.memos.get(&(function, id)) {
                return Seen::Found(addr(&**l.memo_values.get(index).unwrap()));
            }
            if let Some(TrackedSlot::Tombstone) = l.tracked.get(&id) {
                return Seen::Hidden;
            }
        }
        Seen::Miss
    }

    fn walk_tracked(layer: &Layer, id: Id) -> (Seen, bool) {
        for l in layer.chain() {
            match l.tracked.get(&id) {
                Some(TrackedSlot::Entry(index)) => {
                    let value: &(dyn Any + Send + Sync) = &**l.values.get(index).unwrap();
                    return (Seen::Found(addr(value)), std::ptr::eq(l, layer));
                }
                Some(TrackedSlot::Tombstone) => return (Seen::Hidden, false),
                None => {}
            }
        }
        (Seen::Miss, false)
    }

    fn root() -> Arc<Layer> {
        Layer::new(1, Arc::new(SlotCounters::default()), None)
    }

    fn child(parent: &Arc<Layer>) -> Arc<Layer> {
        crate::fork::child_layer(parent)
    }

    fn id(n: u64) -> Id {
        Id::from_bits(n)
    }

    fn function(n: u32) -> IngredientIndex {
        IngredientIndex::new(n)
    }

    fn memo(layer: &Layer, f: u32, n: u64, tag: u64) {
        layer.insert_memo(function(f), id(n), Box::new(TestMemo(tag)));
    }

    /// A xorshift generator, seeded, so a failure is reproducible.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    const FUNCTIONS: u32 = 3;
    const IDS: u64 = 24;

    /// A random tree of layers up to depth 40, with siblings: each layer inserts memos (some keys
    /// twice, as a fixpoint iteration does), tracked entries and tombstones (some ids both, in
    /// either order; some re-created after an older layer's tombstone). Every layer is sealed.
    fn random_tree(seed: u64) -> Vec<Arc<Layer>> {
        let mut rng = Rng(seed);
        let root = root();
        // The root, sealed with no entry.
        root.seal().expect("no handle carries the layer");
        let mut layers = vec![root];
        let mut tag = 0;
        for _ in 0..120 {
            // Most layers go on the newest one, so chains grow deep; the others on any.
            let pick = if rng.below(8) != 0 {
                layers.len() - 1
            } else {
                rng.below(layers.len() as u64) as usize
            };
            let parent = &layers[pick];
            if parent.depth() >= 40 {
                continue;
            }
            let layer = child(parent);
            for _ in 0..rng.below(6) {
                let (f, n) = (rng.below(FUNCTIONS.into()) as u32, 1 + rng.below(IDS));
                tag += 1;
                memo(&layer, f, n, tag);
                if rng.below(4) == 0 {
                    tag += 1;
                    memo(&layer, f, n, tag);
                }
            }
            for _ in 0..rng.below(3) {
                let n = 1 + rng.below(IDS);
                match rng.below(3) {
                    0 => layer.tombstone(id(n)),
                    1 => {
                        layer.insert_tracked(id(n), Box::new(n));
                    }
                    _ => {
                        layer.tombstone(id(n));
                        layer.insert_tracked(id(n), Box::new(n));
                    }
                }
            }
            if rng.below(5) == 0 {
                // A memo and a tombstone of one id in one layer: the memo wins.
                let n = 1 + rng.below(IDS);
                layer.tombstone(id(n));
                tag += 1;
                memo(&layer, 0, n, tag);
            }
            layer.seal().expect("no handle carries the layer");
            layers.push(layer);
        }
        layers
    }

    #[test]
    fn flat_memo_lookup_agrees_with_the_walk() {
        let (mut found, mut hidden, mut deepest) = (0, 0, 0);
        for seed in [0x9E37_79B9_7F4A_7C15, 1, 42, 0xDEAD_BEEF, 7_777_777] {
            let layers = random_tree(seed);
            for layer in &layers {
                deepest = deepest.max(layer.depth());
                // The sealed layer through its view, and an open child of it through the view.
                let open = child(layer);
                for f in 0..FUNCTIONS {
                    for n in 1..=IDS {
                        let want = walk_memo(layer, function(f), id(n));
                        match want {
                            Seen::Found(_) => found += 1,
                            Seen::Hidden => hidden += 1,
                            Seen::Miss => {}
                        }
                        let got = seen_memo(layer.find_memo(function(f), id(n)));
                        assert_eq!(got, want, "seed {seed:#x}, depth {}", layer.depth());
                        let open_got = seen_memo(open.find_memo(function(f), id(n)));
                        assert_eq!(open_got, want, "open child, seed {seed:#x}");
                    }
                }
            }
        }
        // The trees reach deep chains, and their lookups both find and hide.
        assert!(deepest >= 25, "deepest {deepest}");
        assert!(found > 1000 && hidden > 100, "found {found}, hidden {hidden}");
    }

    #[test]
    fn flat_tracked_lookup_agrees_with_the_walk() {
        for seed in [3, 0x0123_4567_89AB_CDEF, 99] {
            let layers = random_tree(seed);
            for layer in &layers {
                let open = child(layer);
                for n in 1..=IDS {
                    assert_eq!(
                        seen_tracked(layer.find_tracked(id(n))),
                        walk_tracked(layer, id(n)),
                        "seed {seed}, depth {}",
                        layer.depth()
                    );
                    assert_eq!(
                        seen_tracked(open.find_tracked(id(n))),
                        walk_tracked(&open, id(n)),
                        "open child, seed {seed}"
                    );
                }
            }
        }
    }

    #[test]
    fn an_open_layers_own_entries_come_first() {
        let root = root();
        memo(&root, 0, 1, 1);
        memo(&root, 0, 2, 2);
        root.seal().unwrap();
        let open = child(&root);
        memo(&open, 0, 1, 3);
        open.tombstone(id(2));
        // Its own memo over the parent's, its own tombstone over the parent's memo.
        let own = seen_memo(open.find_memo(function(0), id(1)));
        assert_eq!(own, walk_memo(&open, function(0), id(1)));
        assert_ne!(own, walk_memo(&root, function(0), id(1)));
        assert_eq!(seen_memo(open.find_memo(function(0), id(2))), Seen::Hidden);
        assert_eq!(seen_memo(open.find_memo(function(1), id(1))), Seen::Miss);
    }

    #[test]
    fn a_query_free_layer_shares_its_parents_view() {
        let parent = root();
        memo(&parent, 0, 1, 1);
        parent.seal().unwrap();
        let free = child(&parent);
        free.seal().unwrap();
        let below = child(&free);
        assert!(Arc::ptr_eq(free.flat_view(), parent.flat_view()));
        assert_eq!(free.stats().flat_bytes, 0);
        assert!(parent.stats().flat_bytes > 0);
        assert_eq!(free.stats().flat_memos, 1);
        // A layer with an entry of its own builds a view of its own.
        below.tombstone(id(1));
        below.seal().unwrap();
        assert!(!Arc::ptr_eq(below.flat_view(), free.flat_view()));
        assert_eq!(below.stats().flat_hidden, 1);
        assert_eq!(below.stats().flat_memos, 0);
    }

    #[test]
    fn a_leaf_builds_no_view() {
        let parent = root();
        parent.seal().unwrap();
        let leaf = child(&parent);
        memo(&leaf, 0, 1, 1);
        leaf.seal().unwrap();
        // Its parent got one when the leaf was made; the leaf gets one only on a lookup in it.
        assert!(parent.flat.get().is_some());
        assert!(leaf.flat.get().is_none());
        let sibling = child(&parent);
        drop(sibling);
        assert!(leaf.flat.get().is_none());
        assert!(matches!(
            leaf.find_memo(function(0), id(1)),
            ChainMemo::Found(_)
        ));
        assert!(leaf.flat.get().is_some());
    }

    #[test]
    fn a_view_of_recomputed_keys_keeps_its_parents_buckets() {
        // Layers that run the same check compute mostly the same keys: the child's view then
        // holds about the parent's entries, in the parent's buckets, not twice as many.
        let parent = root();
        for n in 1..=100 {
            memo(&parent, 0, n, n);
        }
        parent.seal().unwrap();
        let again = child(&parent);
        for n in 1..=101 {
            memo(&again, 0, n, 1000 + n);
        }
        again.seal().unwrap();
        let (mine, theirs) = (again.flat_view(), parent.flat_view());
        assert_eq!(mine.memo_count(), 101);
        assert_eq!(mine.memos.capacity(), theirs.memos.capacity());
        assert_eq!(again.stats().flat_bytes, parent.stats().flat_bytes);
    }

    #[test]
    fn entry_sizes_are_compact() {
        assert_eq!(size_of::<Ref>(), 8);
        let layer = root();
        for n in 1..=100 {
            memo(&layer, 0, n, n);
        }
        layer.tombstone(id(500));
        layer.seal().unwrap();
        let view = layer.flat_view();
        let memos = view.memos.capacity();
        // 100 entries: 128 buckets (capacity 112), 20 bytes each, a control byte each, 16 more.
        assert_eq!(memos, 112);
        let hidden = view.hidden.capacity();
        assert_eq!(hidden, 3);
        let tracked = view.tracked.capacity();
        assert_eq!(tracked, 3);
        assert_eq!(
            view.bytes(),
            (128 * 20 + 128 + 16) + (4 * 8 + 4 + 16) + (4 * 16 + 4 + 16)
        );
        assert_eq!(layer.stats().flat_bytes, view.bytes());
    }

    #[test]
    fn last_written_reads_no_ancestor() {
        // A written root, a write-free middle layer and a top: the top's LOW last-changed
        // revision is the root's, fixed when the top was made (the durability shortcut, D11).
        let root = root();
        root.written.store(true, std::sync::atomic::Ordering::Release);
        root.seal().unwrap();
        let middle = child(&root);
        middle.seal().unwrap();
        let top = child(&middle);
        assert_eq!(top.last_written(), Some(root.revision()));

        // Flags changed behind the sealed ancestors' backs (no path does this; a sealed layer
        // takes no write, E9) are not read: the answer comes from no ancestor. A walk over the
        // chain would find the middle layer.
        middle.written.store(true, std::sync::atomic::Ordering::Release);
        root.written.store(false, std::sync::atomic::Ordering::Release);
        assert_eq!(top.last_written(), Some(root.revision()));
        // The layer's own write counts at once.
        top.written.store(true, std::sync::atomic::Ordering::Release);
        assert_eq!(top.last_written(), Some(top.revision()));
        // A chain with no write has none.
        let fresh = self::root();
        fresh.seal().unwrap();
        assert_eq!(child(&fresh).last_written(), None);
    }
}
