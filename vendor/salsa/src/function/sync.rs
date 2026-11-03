use crossbeam_utils::CachePadded;
use hashbrown::HashTable;
use hashbrown::hash_table::{Entry, OccupiedEntry};
use rustc_hash::FxBuildHasher;
use std::hash::BuildHasher;

// ryo-v2 fork: claims are keyed by the handle's layer (E5); the view gives the layer
use crate::fork::{SharedWriteKind, ZalsaView};
use crate::key::DatabaseKeyIndex;
use crate::plumbing::ZalsaLocal;
use crate::runtime::{
    BlockOnTransferredOwner, BlockResult, BlockTransferredResult, Running, WaitResult,
};
use crate::sync::thread::{self};
use crate::sync::{Mutex, max_parallelism};
use crate::tracing;
// ryo-v2 fork: `Zalsa` is reached through the view (`ZalsaView`)
use crate::{Id, IngredientIndex};

pub(crate) type SyncGuard<'me> = crate::sync::MutexGuard<'me, HashTable<SyncState>>;

/// Tracks the keys that are currently being processed; used to coordinate between
/// worker threads.
pub(crate) struct SyncTable {
    shards: Box<[CachePadded<SyncShard>]>,
    ingredient: IngredientIndex,
}

struct SyncShard {
    syncs: Mutex<HashTable<SyncState>>,

    /// Fits into the shard's cache-line padding and keeps it out of `ClaimGuard`.
    ingredient: IngredientIndex,
}

pub(crate) enum ClaimResult<'a, Guard = ClaimGuard<'a>> {
    /// Successfully claimed the query.
    Claimed(Guard),
    /// Can't claim the query because it is running on an other thread.
    Running(Running<'a>),
    /// Claiming the query results in a cycle.
    Cycle {
        /// `true` if this is a cycle with an inner query. For example, if `a` transferred its ownership to
        /// `b`. If the thread claiming `b` tries to claim `a`, then this results in a cycle except when calling
        /// [`SyncTable::try_claim`] with [`Reentrant::Allow`].
        inner: bool,
    },
}

#[derive(Debug)]
pub(crate) struct SyncState {
    key: Id,

    // ryo-v2 fork: the layer the claim belongs to (0 for a base handle): two layers' claims of
    // one key are distinct (E5)
    layer: usize,

    /// The thread id that currently owns this query (actively executing it or iterating it as part of a larger cycle).
    id: SyncOwner,

    /// Set to true if any other queries are blocked,
    /// waiting for this query to complete.
    anyone_waiting: bool,

    /// Whether any other query has transferred its lock ownership to this query.
    /// This is only an optimization so that the expensive unblocking of transferred queries
    /// can be skipped if `false`. This field might be `true` in cases where queries *were* transferred
    /// to this query, but have since then been transferred to another query (in a later iteration).
    is_transfer_target: bool,

    /// Whether this query has been claimed by the query that currently owns it.
    ///
    /// If `a` has been transferred to `b` and the stack for t1 is `b -> a`, then `a` can be claimed
    /// and `claimed_twice` is set to `true`. However, t2 won't be able to claim `a` because
    /// it doesn't own `b`.
    claimed_twice: bool,
}

// ryo-v2 fork: the hash of a claim key; a base handle's (layer 0) is upstream's
#[inline]
fn claim_hash(layer: usize, key_index: Id) -> u64 {
    if layer == 0 {
        FxBuildHasher.hash_one(key_index)
    } else {
        FxBuildHasher.hash_one((layer, key_index))
    }
}

impl SyncTable {
    pub(crate) fn new(ingredient: IngredientIndex) -> Self {
        let shard_count = max_parallelism().next_power_of_two().max(2);

        Self {
            shards: (0..shard_count)
                .map(|_| {
                    CachePadded::new(SyncShard {
                        syncs: Mutex::default(),
                        ingredient,
                    })
                })
                .collect(),
            ingredient,
        }
    }

    #[inline]
    fn shard_index(&self, hash: u64) -> usize {
        let hash = hash as usize;
        let shift = usize::BITS - self.shards.len().trailing_zeros();
        (hash << 7) >> shift
    }

    #[inline]
    fn shard_for(&self, hash: u64) -> &SyncShard {
        &self.shards[self.shard_index(hash)]
    }

    /// Claims the given key index, or blocks if it is running on another thread.
    pub(crate) fn try_claim<'me>(
        &'me self,
        // ryo-v2 fork: the view, whose layer keys the claim (E5)
        zalsa: &'me ZalsaView,
        zalsa_local: &'me ZalsaLocal,
        key_index: Id,
        reentrant: Reentrancy,
    ) -> ClaimResult<'me> {
        // ryo-v2 fork: claims are keyed by (layer, key)
        let layer = zalsa.layer_key();
        let hash = claim_hash(layer, key_index);
        let shard = self.shard_for(hash);
        let mut write = shard.syncs.lock();
        match write.entry(
            hash,
            // ryo-v2 fork: claims are keyed by (layer, key)
            |state| state.key == key_index && state.layer == layer,
            |state| claim_hash(state.layer, state.key),
        ) {
            Entry::Occupied(occupied_entry) => {
                let id = match occupied_entry.get().id {
                    SyncOwner::Thread(id) => id,
                    SyncOwner::Transferred => {
                        return match self.try_claim_transferred(
                            zalsa,
                            zalsa_local,
                            occupied_entry,
                            shard,
                            reentrant,
                        ) {
                            Ok(claimed) => claimed,
                            Err(other_thread) => match other_thread.block(write) {
                                BlockResult::Cycle => ClaimResult::Cycle { inner: false },
                                BlockResult::Running(running) => ClaimResult::Running(running),
                            },
                        };
                    }
                };

                let SyncState { anyone_waiting, .. } = occupied_entry.into_mut();

                // NB: `Ordering::Relaxed` is sufficient here,
                // as there are no loads that are "gated" on this
                // value. Everything that is written is also protected
                // by a lock that must be acquired. The role of this
                // boolean is to decide *whether* to acquire the lock,
                // not to gate future atomic reads.
                *anyone_waiting = true;
                match zalsa.runtime().block(
                    // ryo-v2 fork: the graph of the handle's layer (E5)
                    zalsa.dependency_graph(),
                    DatabaseKeyIndex::new(self.ingredient, key_index),
                    id,
                    write,
                ) {
                    BlockResult::Running(blocked_on) => ClaimResult::Running(blocked_on),
                    BlockResult::Cycle => ClaimResult::Cycle { inner: false },
                }
            }
            Entry::Vacant(vacant_entry) => {
                // ryo-v2 fork: a claim under a layer is a counted shared write of the base's
                // sync table (E2)
                if layer != 0 {
                    zalsa.fork().record_shared_write(SharedWriteKind::SyncClaim);
                }
                vacant_entry.insert(SyncState {
                    key: key_index,
                    // ryo-v2 fork: the claim's layer
                    layer,
                    id: SyncOwner::Thread(thread::current().id()),
                    anyone_waiting: false,
                    is_transfer_target: false,
                    claimed_twice: false,
                });
                ClaimResult::Claimed(ClaimGuard {
                    key_index,
                    zalsa,
                    zalsa_local,
                    shard,
                    mode: ReleaseMode::Default,
                })
            }
        }
    }

    /// Claims the given key index, or blocks if it is running on another thread.
    pub(crate) fn peek_claim<'me>(
        &'me self,
        // ryo-v2 fork: the view, whose layer keys the claim (E5)
        zalsa: &'me ZalsaView,
        key_index: Id,
        reentrant: Reentrancy,
    ) -> ClaimResult<'me, ()> {
        // ryo-v2 fork: claims are keyed by (layer, key)
        let layer = zalsa.layer_key();
        let hash = claim_hash(layer, key_index);
        let mut write = self.shard_for(hash).syncs.lock();
        // ryo-v2 fork: claims are keyed by (layer, key)
        match write.find_entry(hash, |state| state.key == key_index && state.layer == layer) {
            Ok(occupied_entry) => {
                let id = match occupied_entry.get().id {
                    SyncOwner::Thread(id) => id,
                    SyncOwner::Transferred => {
                        return match self.peek_claim_transferred(zalsa, occupied_entry, reentrant) {
                            Ok(claimed) => claimed,
                            Err(other_thread) => match other_thread.block(write) {
                                BlockResult::Cycle => ClaimResult::Cycle { inner: false },
                                BlockResult::Running(running) => ClaimResult::Running(running),
                            },
                        };
                    }
                };

                let SyncState { anyone_waiting, .. } = occupied_entry.into_mut();

                // NB: `Ordering::Relaxed` is sufficient here,
                // as there are no loads that are "gated" on this
                // value. Everything that is written is also protected
                // by a lock that must be acquired. The role of this
                // boolean is to decide *whether* to acquire the lock,
                // not to gate future atomic reads.
                *anyone_waiting = true;
                match zalsa.runtime().block(
                    // ryo-v2 fork: the graph of the handle's layer (E5)
                    zalsa.dependency_graph(),
                    DatabaseKeyIndex::new(self.ingredient, key_index),
                    id,
                    write,
                ) {
                    BlockResult::Running(blocked_on) => ClaimResult::Running(blocked_on),
                    BlockResult::Cycle => ClaimResult::Cycle { inner: false },
                }
            }
            Err(_) => ClaimResult::Claimed(()),
        }
    }

    #[cold]
    #[inline(never)]
    fn try_claim_transferred<'me>(
        &'me self,
        // ryo-v2 fork: the view, whose layer's graph holds the transfer (E5)
        zalsa: &'me ZalsaView,
        zalsa_local: &'me ZalsaLocal,
        mut entry: OccupiedEntry<SyncState>,
        shard: &'me SyncShard,
        reentrant: Reentrancy,
    ) -> Result<ClaimResult<'me>, Box<BlockOnTransferredOwner<'me>>> {
        let key_index = entry.get().key;
        let database_key_index = DatabaseKeyIndex::new(self.ingredient, key_index);
        let thread_id = thread::current().id();

        match zalsa
            .runtime()
            // ryo-v2 fork: the graph of the handle's layer (E5)
            .block_transferred(zalsa.dependency_graph(), database_key_index, thread_id)
        {
            BlockTransferredResult::ImTheOwner if reentrant.is_allow() => {
                let SyncState {
                    id, claimed_twice, ..
                } = entry.into_mut();
                debug_assert!(!*claimed_twice);

                *id = SyncOwner::Thread(thread_id);
                *claimed_twice = true;

                Ok(ClaimResult::Claimed(ClaimGuard {
                    key_index,
                    zalsa,
                    zalsa_local,
                    shard,
                    mode: ReleaseMode::SelfOnly,
                }))
            }
            BlockTransferredResult::ImTheOwner => Ok(ClaimResult::Cycle { inner: true }),
            BlockTransferredResult::OwnedBy(other_thread) => {
                entry.get_mut().anyone_waiting = true;
                Err(other_thread)
            }
            BlockTransferredResult::Released => {
                *entry.get_mut() = SyncState {
                    key: key_index,
                    // ryo-v2 fork: the claim's layer
                    layer: zalsa.layer_key(),
                    id: SyncOwner::Thread(thread_id),
                    anyone_waiting: false,
                    is_transfer_target: false,
                    claimed_twice: false,
                };
                Ok(ClaimResult::Claimed(ClaimGuard {
                    key_index,
                    zalsa,
                    zalsa_local,
                    shard,
                    mode: ReleaseMode::Default,
                }))
            }
        }
    }

    #[cold]
    #[inline(never)]
    fn peek_claim_transferred<'me>(
        &'me self,
        // ryo-v2 fork: the view, whose layer's graph holds the transfer (E5)
        zalsa: &'me ZalsaView,
        mut entry: OccupiedEntry<SyncState>,
        reentrant: Reentrancy,
    ) -> Result<ClaimResult<'me, ()>, Box<BlockOnTransferredOwner<'me>>> {
        let key_index = entry.get().key;
        let database_key_index = DatabaseKeyIndex::new(self.ingredient, key_index);
        let thread_id = thread::current().id();

        match zalsa
            .runtime()
            // ryo-v2 fork: the graph of the handle's layer (E5)
            .block_transferred(zalsa.dependency_graph(), database_key_index, thread_id)
        {
            BlockTransferredResult::ImTheOwner if reentrant.is_allow() => {
                Ok(ClaimResult::Claimed(()))
            }
            BlockTransferredResult::ImTheOwner => Ok(ClaimResult::Cycle { inner: true }),
            BlockTransferredResult::OwnedBy(other_thread) => {
                entry.get_mut().anyone_waiting = true;
                Err(other_thread)
            }
            BlockTransferredResult::Released => Ok(ClaimResult::Claimed(())),
        }
    }

    /// Marks `key_index` as a transfer target.
    ///
    /// Returns the `SyncOwnerId` of the thread that currently owns this query.
    ///
    /// Note: The result of this method will immediately become stale unless the thread owning `key_index`
    /// is currently blocked on this thread (claiming `key_index` from this thread results in a cycle).
    pub(super) fn mark_as_transfer_target(
        &self,
        // ryo-v2 fork: the layer whose claim of `key_index` is meant (E5)
        layer: usize,
        key_index: Id,
    ) -> Option<SyncOwner> {
        // ryo-v2 fork: claims are keyed by (layer, key)
        let hash = claim_hash(layer, key_index);
        let mut syncs = self.shard_for(hash).syncs.lock();
        syncs
            // ryo-v2 fork: claims are keyed by (layer, key)
            .find_mut(hash, |state| state.key == key_index && state.layer == layer)
            .map(|state| {
                // We set `anyone_waiting` to true because it is used in `ClaimGuard::release`
                // to exit early if the query doesn't need to release any locks.
                // However, there are now dependent queries that need to be released, that's why we set `anyone_waiting` to true,
                // so that `ClaimGuard::release` no longer exits early.
                state.anyone_waiting = true;
                state.is_transfer_target = true;

                state.id
            })
    }
}

#[derive(Copy, Clone, Debug)]
pub enum SyncOwner {
    /// Query is owned by this thread
    Thread(thread::ThreadId),

    /// The query's lock ownership has been transferred to another query.
    /// E.g. if `a` transfers its ownership to `b`, then only the thread in the critical path
    /// to complete `b` can claim `a` (in most instances, only the thread owning `b` can claim `a`).
    ///
    /// The thread owning `a` is stored in the `DependencyGraph`.
    ///
    /// A query can be marked as `Transferred` even if it has since then been released by the owning query.
    /// In that case, the query is effectively unclaimed and the `Transferred` state is stale. The reason
    /// for this is that it avoids the need for locking each sync table when releasing the transferred queries.
    Transferred,
}

/// Marks an active 'claim' in the synchronization map. The claim is
/// released when this value is dropped.
#[must_use]
pub(crate) struct ClaimGuard<'me> {
    key_index: Id,
    // ryo-v2 fork: the view, whose layer keys the claim (E5)
    zalsa: &'me ZalsaView,
    shard: &'me SyncShard,
    mode: ReleaseMode,
    zalsa_local: &'me ZalsaLocal,
}

impl<'me> ClaimGuard<'me> {
    // ryo-v2 fork: the view, whose layer the claimed query runs in
    pub(crate) const fn zalsa(&self) -> &'me ZalsaView {
        self.zalsa
    }

    pub(crate) fn zalsa_local(&self) -> &'me ZalsaLocal {
        self.zalsa_local
    }

    pub(crate) const fn database_key_index(&self) -> DatabaseKeyIndex {
        DatabaseKeyIndex::new(self.shard.ingredient, self.key_index)
    }

    pub(crate) fn set_release_mode(&mut self, mode: ReleaseMode) {
        self.mode = mode;
    }

    #[inline]
    fn hash(&self) -> u64 {
        // ryo-v2 fork: claims are keyed by (layer, key)
        claim_hash(self.zalsa.layer_key(), self.key_index)
    }

    // ryo-v2 fork: whether a sync-table entry is this guard's claim (same key, same layer)
    #[inline]
    fn is_mine(&self, state: &SyncState) -> bool {
        state.key == self.key_index && state.layer == self.zalsa.layer_key()
    }

    #[cold]
    #[inline(never)]
    fn release_panicking(&self) {
        let hash = self.hash();
        let mut syncs = self.shard.syncs.lock();
        let state = syncs
            // ryo-v2 fork: claims are keyed by (layer, key)
            .find_entry(hash, |state| self.is_mine(state))
            .expect("key claimed twice?")
            .remove()
            .0;
        // ryo-v2 fork: a cancelled ancestor layer stops this layer's queries as cancelled too
        let result = if self
            .zalsa
            .should_trigger_local_cancellation(self.zalsa_local)
        {
            WaitResult::Cancelled
        } else {
            WaitResult::Panicked
        };
        tracing::debug!(
            "Release claim on {:?} due to {:?}",
            self.database_key_index(),
            result
        );
        self.release(state, result);
    }

    #[inline(always)]
    fn release(&self, state: SyncState, wait_result: WaitResult) {
        let SyncState {
            anyone_waiting,
            is_transfer_target,
            claimed_twice,
            ..
        } = state;

        if !anyone_waiting {
            return;
        }

        let runtime = self.zalsa.runtime();
        let database_key_index = self.database_key_index();
        // ryo-v2 fork: the graph of the handle's layer (E5)
        let dependency_graph = self.zalsa.dependency_graph();

        if claimed_twice {
            // ryo-v2 fork: in the layer's graph
            runtime.undo_transfer_lock(dependency_graph, database_key_index);
        }

        // ryo-v2 fork: in the layer's graph
        runtime.unblock_queries_blocked_on(dependency_graph, database_key_index, wait_result);

        if is_transfer_target {
            // ryo-v2 fork: in the layer's graph
            runtime.unblock_transferred_queries_owned_by(
                dependency_graph,
                database_key_index,
                wait_result,
            );
        }
    }

    #[cold]
    #[inline(never)]
    fn release_self(&self) {
        let hash = self.hash();
        let mut syncs = self.shard.syncs.lock();
        // ryo-v2 fork: claims are keyed by (layer, key)
        let Ok(mut state) = syncs.find_entry(hash, |state| self.is_mine(state)) else {
            panic!("key should only be claimed/released once");
        };

        if state.get().claimed_twice {
            state.get_mut().claimed_twice = false;
            state.get_mut().id = SyncOwner::Transferred;
        } else {
            self.release(state.remove().0, WaitResult::Completed);
        }
    }

    #[cold]
    #[inline(never)]
    pub(crate) fn transfer(&self, new_owner: DatabaseKeyIndex) -> bool {
        let hash = self.hash();

        // Get the owning thread of `new_owner`.
        // The thread id is guaranteed to not be stale because `new_owner` must be blocked on `self_key`
        // or `transfer_lock` will panic (at least in debug builds).
        let Some(new_owner_thread_id) = self
            .zalsa
            .lookup_ingredient(new_owner.ingredient_index())
            .as_function()
            .expect("lock owners must be function ingredients")
            .sync_table()
            // ryo-v2 fork: the new owner's claim in this guard's layer (E5)
            .mark_as_transfer_target(self.zalsa.layer_key(), new_owner.key_index())
        else {
            self.release(
                self.shard
                    .syncs
                    .lock()
                    // ryo-v2 fork: claims are keyed by (layer, key)
                    .find_entry(hash, |state| self.is_mine(state))
                    .expect("key should only be claimed/released once")
                    .remove()
                    .0,
                WaitResult::Panicked,
            );

            panic!("new owner to be a locked query")
        };

        let mut syncs = self.shard.syncs.lock();

        let self_key = self.database_key_index();
        tracing::debug!(
            "Transferring lock ownership of {self_key:?} to {new_owner:?} ({new_owner_thread_id:?})"
        );

        let SyncState {
            id, claimed_twice, ..
        } = syncs
            // ryo-v2 fork: claims are keyed by (layer, key)
            .find_mut(hash, |state| self.is_mine(state))
            .expect("key should only be claimed/released once");

        *id = SyncOwner::Transferred;
        *claimed_twice = false;

        self.zalsa.runtime().transfer_lock(
            // ryo-v2 fork: the graph of the handle's layer (E5)
            self.zalsa.dependency_graph(),
            self_key,
            new_owner,
            new_owner_thread_id,
            syncs,
        )
    }

    /// Drops the claim on the memo.
    ///
    /// Returns `true` if the lock was transferred to another query and
    /// this thread blocked waiting for the new owner's lock to be released.
    /// In that case, any computed memo need to be refetched because they may have
    /// changed since `drop` was called.
    pub(crate) fn drop(mut self) -> bool {
        let refetch = self.drop_impl();
        std::mem::forget(self);
        refetch
    }

    fn drop_impl(&mut self) -> bool {
        match self.mode {
            ReleaseMode::Default => {
                let hash = self.hash();
                let mut syncs = self.shard.syncs.lock();
                let state = syncs
                    // ryo-v2 fork: claims are keyed by (layer, key)
                    .find_entry(hash, |state| self.is_mine(state))
                    .expect("key should only be claimed/released once")
                    .remove()
                    .0;

                self.release(state, WaitResult::Completed);
                false
            }
            ReleaseMode::SelfOnly => {
                self.release_self();
                false
            }
            ReleaseMode::TransferTo(new_owner) => self.transfer(new_owner),
        }
    }
}

impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        if thread::panicking() {
            self.release_panicking();
            return;
        }

        self.drop_impl();
    }
}

impl std::fmt::Debug for SyncTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncTable").finish()
    }
}

/// Controls how the lock is released when the `ClaimGuard` is dropped.
#[derive(Copy, Clone, Debug, Default)]
pub(crate) enum ReleaseMode {
    /// The default release mode.
    ///
    /// Releases the query for which this claim guard holds the lock and any queries that have
    /// transferred ownership to this query.
    #[default]
    Default,

    /// Only releases the lock for this query. Any query that has transferred ownership to this query
    /// will remain locked.
    ///
    /// If this thread panics, the query will be released as normal (default mode).
    SelfOnly,

    /// Transfers the ownership of the lock to the specified query.
    ///
    /// The query will remain locked and only the thread owning the transfer target will be resumed.
    ///
    /// The transfer target must be a query that's blocked on this query to guarantee that the transfer target doesn't complete
    /// before the transfer is finished (which would leave this query locked forever).
    ///
    /// If this thread panics, the query will be released as normal (default mode).
    TransferTo(DatabaseKeyIndex),
}

impl std::fmt::Debug for ClaimGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaimGuard")
            .field("key_index", &self.key_index)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

#[cfg(all(not(feature = "shuttle"), target_pointer_width = "64"))]
const _: [(); std::mem::size_of::<ClaimGuard<'static>>()] = [(); std::mem::size_of::<[usize; 6]>()];

#[cfg(all(not(feature = "shuttle"), target_pointer_width = "64"))]
const _: [(); std::mem::size_of::<CachePadded<SyncShard>>()] =
    [(); std::mem::size_of::<CachePadded<Mutex<HashTable<SyncState>>>>()];

/// Controls whether this thread can claim a query that transferred its ownership to a query
/// this thread currently holds the lock for.
///
/// For example: if query `a` transferred its ownership to query `b`, and this thread holds
/// the lock for `b`, then this thread can also claim `a` — but only when using [`Self::Allow`].
#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) enum Reentrancy {
    /// Allow `try_claim` to reclaim a query's that transferred its ownership to a query
    /// hold by this thread.
    Allow,

    /// Only allow claiming queries that haven't been claimed by any thread.
    Deny,
}

impl Reentrancy {
    const fn is_allow(self) -> bool {
        matches!(self, Reentrancy::Allow)
    }
}
