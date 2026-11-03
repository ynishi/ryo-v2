// ryo-v2 fork: the per-layer stores of the fork Engine (D11). Not upstream salsa code.
//! The append-only stores a [`Layer`](super::Layer) keeps: a sharded index from a key to a slot of
//! an append-only vector of boxed values. A value pushed stays at its address until the layer is
//! dropped, so a reference handed out from it lives as long as the layer; a key re-pointed at a
//! newer value leaves the older one in place.

use std::hash::{BuildHasher, Hash};

use parking_lot::RwLock;
use rustc_hash::{FxBuildHasher, FxHashMap};

const SHARDS: usize = 32;

/// A map from `K` to `V`, sharded by the key's hash, each shard under a read-write lock that is
/// held only for one lookup or insert (no user code runs under it).
pub(crate) struct Sharded<K, V> {
    shards: Box<[RwLock<FxHashMap<K, V>>]>,
}

impl<K: Hash + Eq, V: Copy> Sharded<K, V> {
    pub(crate) fn new() -> Self {
        Sharded {
            shards: (0..SHARDS).map(|_| RwLock::default()).collect(),
        }
    }

    #[inline]
    fn shard(&self, key: &K) -> &RwLock<FxHashMap<K, V>> {
        let hash = FxBuildHasher.hash_one(key) as usize;
        &self.shards[(hash >> 7) % SHARDS]
    }

    #[inline]
    pub(crate) fn get(&self, key: &K) -> Option<V> {
        self.shard(key).read().get(key).copied()
    }

    pub(crate) fn insert(&self, key: K, value: V) -> Option<V> {
        self.shard(&key).write().insert(key, value)
    }
}
