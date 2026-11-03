#![allow(clippy::undocumented_unsafe_blocks)] // TODO(#697) document safety

use std::any::TypeId;
use std::hash::Hash;
use std::marker::PhantomData;
use std::ops::Index;
use std::{fmt, iter, mem};

use crossbeam_queue::SegQueue;
use hashbrown::hash_table::Entry;
use thin_vec::ThinVec;
use tracked_field::FieldIngredientImpl;

// ryo-v2 fork: the entry points take the handle's view; under a layer the layer holds entries
use crate::fork::{ChainMemo, ChainTracked, Layer, LayerFreeList, SharedWriteKind, ZalsaView};
use crate::function::VerifyResult;
use crate::hash::{FxHashSet, FxIndexSet};
use crate::id::{AsId, FromId};
use crate::ingredient::{Ingredient, Jar};
use crate::key::DatabaseKeyIndex;
use crate::plumbing::{self, ZalsaLocal};
use crate::revision::{AtomicRevision, OptionalAtomicRevision};
use crate::runtime::Stamp;
use crate::salsa_struct::SalsaStructInDb;
use crate::sync::Arc;
use crate::table::memo::{MemoTable, MemoTableTypes, MemoTableWithTypesMut};
use crate::table::{Slot, Table};
use crate::zalsa::{IngredientIndex, JarKind, Zalsa};
use crate::zalsa_local::QueryEdge;
use crate::{Durability, Event, EventKind, Id, Revision};

pub mod tracked_field;

// ANCHOR: Configuration
/// Trait that defines the key properties of a tracked struct.
///
/// Implemented by the `#[salsa::tracked]` macro when applied
/// to a struct.
///
/// # Safety
///
/// For every lifetime `'db`, `Fields<'db>` must be safe for Salsa to retain
/// as `Fields<'static>` and later expose with the lifetime of a database
/// borrow. Both types must have identical layouts and validity invariants.
pub unsafe trait Configuration: Sized + 'static {
    const LOCATION: crate::ingredient::Location;

    /// The debug name of the tracked struct.
    const DEBUG_NAME: &'static str;

    /// The debug names of any tracked fields.
    const TRACKED_FIELD_NAMES: &'static [&'static str];

    /// The relative indices of any tracked fields.
    const TRACKED_FIELD_INDICES: &'static [usize];

    /// Whether this struct should be persisted with the database.
    const PERSIST: bool;

    /// A (possibly empty) tuple of the fields for this struct.
    type Fields<'db>: Send + Sync;

    /// A array of [`AtomicRevision`][] values, one per each of the tracked value fields.
    /// When a struct is re-recreated in a new revision, the corresponding
    /// entries for each field are updated to the new revision if their
    /// values have changed (or if the field is marked as `#[no_eq]`).
    #[cfg(feature = "persistence")]
    type Revisions: Send
        + Sync
        + Index<usize, Output = AtomicRevision>
        + plumbing::serde::Serialize
        + for<'de> plumbing::serde::Deserialize<'de>;

    #[cfg(not(feature = "persistence"))]
    type Revisions: Send + Sync + Index<usize, Output = AtomicRevision>;

    type Struct<'db>: Copy + FromId + AsId;

    fn untracked_fields(fields: &Self::Fields<'_>) -> impl Hash;

    /// Create a new value revision array where each element is set to `current_revision`.
    fn new_revisions(current_revision: Revision) -> Self::Revisions;

    /// Update the field data and, if the value has changed,
    /// the appropriate entry in the `revisions` array (tracked fields only).
    ///
    /// Returns `true` if any identity field changed and the struct should be
    /// considered re-created.
    ///
    fn update_fields<'db>(
        current_revision: Revision,
        revisions: &Self::Revisions,
        old_fields: &mut Self::Fields<'db>,
        new_fields: Self::Fields<'db>,
    ) -> bool;

    // ryo-v2 fork: `update_fields` without the update, for a re-run under a layer (D11): sets
    // `changed[i]` for each tracked field `i` (relative index) that differs between `old_fields`
    // and `new_fields` by the field's own equality, and returns `true` if an identity field
    // differs. Compares by reference, so the fields need no `Clone`.
    /// Which tracked fields differ, and whether an identity field differs (ryo-v2 fork).
    fn fields_changed<'db>(
        old_fields: &Self::Fields<'db>,
        new_fields: &Self::Fields<'db>,
        changed: &mut [bool],
    ) -> bool;

    /// Returns the size of any heap allocations in the output value, in bytes.
    fn heap_size(_value: &Self::Fields<'_>) -> Option<usize> {
        None
    }

    /// Serialize the fields using `serde`.
    ///
    /// Panics if the value is not persistable, i.e. `Configuration::PERSIST` is `false`.
    fn serialize<S>(value: &Self::Fields<'_>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: plumbing::serde::Serializer;

    /// Deserialize the fields using `serde`.
    ///
    /// Panics if the value is not persistable, i.e. `Configuration::PERSIST` is `false`.
    fn deserialize<'de, D>(deserializer: D) -> Result<Self::Fields<'static>, D::Error>
    where
        D: plumbing::serde::Deserializer<'de>;
}
// ANCHOR_END: Configuration

/// Replaces a recreated field when `old_value` and `new_value` are not equal.
#[doc(hidden)]
pub fn update_field<T>(
    old_value: &mut T,
    new_value: T,
    values_equal: impl FnOnce(&T, &T) -> bool,
) -> bool {
    if values_equal(old_value, &new_value) {
        return false;
    }

    *old_value = new_value;
    true
}

// ryo-v2 fork: whether a re-created field differs, by the field's equality (`update_field`
// without the replacement), for `Configuration::fields_changed`
/// Whether `old_value` and `new_value` differ by `values_equal` (ryo-v2 fork).
#[doc(hidden)]
pub fn field_changed<T>(
    old_value: &T,
    new_value: &T,
    values_equal: impl FnOnce(&T, &T) -> bool,
) -> bool {
    !values_equal(old_value, new_value)
}

// ryo-v2 fork: a tracked struct as a layer holds it after its creator re-ran under the layer
// (D11): the new fields, per tracked field the revision it last changed at, and the durability.
// Its `updated_at` is the layer's revision (an entry is written only in its own layer).
pub(crate) struct LayerEntry<C: Configuration> {
    fields: C::Fields<'static>,
    revisions: Box<[Revision]>,
    durability: Durability,
}

// ryo-v2 fork: a layer's tracked-struct entry, as the struct's type
fn downcast_entry<C: Configuration>(value: &(dyn std::any::Any + Send + Sync)) -> &LayerEntry<C> {
    value
        .downcast_ref::<LayerEntry<C>>()
        .expect("ryo-v2 fork: a tracked-struct entry has its ingredient's type")
}

// ryo-v2 fork: the revision of tracked field `relative` in a layer's entry
pub(crate) fn entry_revision<C: Configuration>(
    value: &(dyn std::any::Any + Send + Sync),
    relative: usize,
) -> Revision {
    downcast_entry::<C>(value).revisions[relative]
}

// ryo-v2 fork: retained fields with the database lifetime of the borrow, as upstream's
// `lock_fields` and `Value::fields` give them
fn restore_fields<'db, C: Configuration>(fields: &'db C::Fields<'static>) -> &'db C::Fields<'db> {
    // SAFETY: Guaranteed by `Configuration`; the restored lifetime is bounded by the borrow of
    // the retained fields.
    unsafe { std::mem::transmute::<&C::Fields<'static>, &C::Fields<'db>>(fields) }
}

// ryo-v2 fork: a tracked struct as a layered handle reads it
enum LayerView<'db, C: Configuration> {
    /// An entry of a layer of the chain.
    Entry(&'db LayerEntry<C>),
    /// The base slot (no layer of the chain re-created the struct); its `updated_at` is `Some`.
    Base(*const Value<C>),
}

pub struct JarImpl<C>
where
    C: Configuration,
{
    phantom: PhantomData<C>,
}

impl<C: Configuration> Default for JarImpl<C> {
    fn default() -> Self {
        Self {
            phantom: Default::default(),
        }
    }
}

impl<C: Configuration> Jar for JarImpl<C> {
    fn create_ingredients(
        _zalsa: &mut Zalsa,
        struct_index: crate::zalsa::IngredientIndex,
    ) -> Vec<Box<dyn Ingredient>> {
        let struct_ingredient = <IngredientImpl<C>>::new(struct_index);

        let tracked_field_ingredients =
            C::TRACKED_FIELD_INDICES
                .iter()
                .copied()
                .map(|tracked_index| {
                    Box::new(<FieldIngredientImpl<C>>::new(
                        tracked_index,
                        struct_index.successor(tracked_index),
                    )) as _
                });

        iter::once(Box::new(struct_ingredient) as _)
            .chain(tracked_field_ingredients)
            .collect()
    }

    fn id_struct_type_id() -> TypeId {
        TypeId::of::<C::Struct<'static>>()
    }
}

pub trait TrackedStructInDb: SalsaStructInDb {
    /// Converts the identifier for this tracked struct into a `DatabaseKeyIndex`.
    fn database_key_index(zalsa: &Zalsa, id: Id) -> DatabaseKeyIndex;
}

/// Created for each tracked struct.
///
/// This ingredient only stores the "id" fields. It is a kind of "dressed up" interner;
/// the active query + values of id fields are hashed to create the tracked
/// struct id. The value fields are stored in [`crate::function::IngredientImpl`]
/// instances keyed by the tracked struct id.
///
/// Unlike normal interned values, tracked struct indices can be deleted and reused aggressively
/// without dependency edges on the creating query. When a tracked function is collected,
/// any tracked structs it created can be deleted. Additionally, when a tracked function
/// re-executes but does not create a tracked struct that was previously created, it can
/// be deleted. No dependency edge is required as the lifetime of a tracked struct is tied
/// directly to the query that created it.
pub struct IngredientImpl<C>
where
    C: Configuration,
{
    /// Our index in the database.
    ingredient_index: IngredientIndex,

    /// Phantom data: we fetch `Value<C>` out from `Table`
    phantom: PhantomData<fn() -> Value<C>>,

    /// Store freed ids
    free_list: SegQueue<Id>,

    // ryo-v2 fork: the slots dropped layers gave back (structs they created with a new
    // identity), shared with the layers; taken again only under a layer, and moved to
    // `free_list` at a thaw
    layer_free: Arc<LayerFreeList>,

    memo_table_types: Arc<MemoTableTypes>,
}

/// Defines the identity of a tracked struct.
/// This is the key to a hashmap that is (initially)
/// stored in the [`ActiveQuery`](`crate::active_query::ActiveQuery`)
/// struct and later moved to the [`Memo`](`crate::function::memo::Memo`).
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Copy, Clone)]
#[cfg_attr(feature = "persistence", derive(serde::Serialize, serde::Deserialize))]
pub(crate) struct Identity {
    // Conceptually, this contains an `IdentityHash`, but using `IdentityHash` directly will grow the size
    // of this struct struct by a `std::mem::size_of::<usize>()` due to unusable padding. To avoid this increase
    // in size, we inline the fields of `IdentityHash`.
    /// Index of the tracked struct ingredient.
    ingredient_index: IngredientIndex,

    /// Hash of the id fields.
    hash: u64,

    /// The unique disambiguator assigned within the active query
    /// to distinguish distinct tracked structs with the same identity_hash.
    disambiguator: Disambiguator,
}

impl Identity {
    pub(crate) fn ingredient_index(&self) -> IngredientIndex {
        self.ingredient_index
    }
}

/// Stores the data that (almost) uniquely identifies a tracked struct.
///
/// This includes the ingredient index of that struct type plus the hash of its untracked
/// fields. This is mapped to a disambiguator -- a value that starts as 0 but increments
/// each round, allowing for multiple tracked structs with the same hash and `IngredientIndex`
/// created within the query to each have a unique ID.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Copy, Clone)]
pub struct IdentityHash {
    /// Index of the tracked struct ingredient.
    ingredient_index: IngredientIndex,

    /// Hash of the id fields.
    hash: u64,
}

/// A map from tracked struct [`Identity`] to their final [`Id`].
#[derive(Default, Debug)]
pub(crate) struct IdentityMap {
    // We use a `HashTable` here as our key contains its own hash (`Identity::hash`),
    // so we do the hash wrangling ourselves.
    table: hashbrown::HashTable<TrackedEntry>,
}

impl IdentityMap {
    /// Seeds the identity map with the IDs from a previous revision.
    pub(crate) fn seed(&mut self, source: &[(Identity, Id)]) {
        for &(key, id) in source {
            self.insert_entry(key, id, false);
        }
    }

    // Mark all tracked structs in the map as created by the current query.
    pub(crate) fn mark_all_active(&mut self, items: impl IntoIterator<Item = (Identity, Id)>) {
        for (key, id) in items {
            self.insert_entry(key, id, true);
        }
    }

    /// Insert a tracked struct identity into the map with the given ID.
    pub(crate) fn insert(&mut self, key: Identity, id: Id) -> Option<Id> {
        self.insert_entry(key, id, true)
    }

    fn insert_entry(&mut self, key: Identity, id: Id, active: bool) -> Option<Id> {
        let entry = self.table.entry(
            key.hash,
            |entry| entry.identity == key,
            |entry| entry.identity.hash,
        );

        match entry {
            Entry::Vacant(entry) => {
                entry.insert(TrackedEntry {
                    identity: key,
                    id,
                    active,
                });

                None
            }
            Entry::Occupied(mut entry) => {
                let tracked = entry.get_mut();
                tracked.active = active;

                Some(std::mem::replace(&mut tracked.id, id))
            }
        }
    }

    /// Reuses an existing identity if it already exists in the map, marking it as active.
    ///
    /// Returns the existing ID, or `None` if no ID for the given identity exists.
    pub(crate) fn reuse(&mut self, key: &Identity) -> Option<Id> {
        self.table
            .find_mut(key.hash, |entry| key == &entry.identity)
            .map(|entry| {
                entry.active = true;
                entry.id
            })
    }

    /// Returns `true` if the given tracked struct key was created in the current query execution.
    pub(crate) fn is_active(&self, key: DatabaseKeyIndex) -> bool {
        self.table
            .iter()
            .find(|entry| {
                entry.id == key.key_index()
                    && entry.identity.ingredient_index() == key.ingredient_index()
            })
            .is_some_and(|entry| entry.active)
    }

    /// Drains the [`IdentityMap`] into a tuple of active and stale tracked structs.
    ///
    /// The first entry contains the identity and IDs of any tracked structs that were
    /// created by the current execution of the query, while the second entry contains any
    /// tracked structs that were created in a previous execution but not the current one.
    #[expect(clippy::type_complexity)]
    pub(crate) fn drain(&mut self) -> (ThinVec<(Identity, Id)>, Vec<(Identity, Id)>) {
        if self.table.is_empty() {
            return (ThinVec::new(), Vec::new());
        }

        let mut stale = Vec::new();
        let mut active = ThinVec::with_capacity(self.table.len());

        for entry in self.table.drain() {
            if entry.active {
                active.push((entry.identity, entry.id));
            } else {
                stale.push((entry.identity, entry.id));
            }
        }

        // Removing a stale tracked struct ID shows up in the event logs, so make sure
        // the order is stable here.
        stale.sort_unstable_by(|a, b| {
            (a.0.ingredient_index(), a.1).cmp(&(b.0.ingredient_index(), b.1))
        });

        (active, stale)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.table.clear()
    }
}

/// A tracked struct entry stored in an [`IdentityMap`].
#[derive(Debug)]
struct TrackedEntry {
    /// The identity of the tracked struct.
    identity: Identity,

    /// The current ID of the tracked struct.
    id: Id,

    /// Whether or not this tracked struct was created by the current query.
    ///
    /// Entries where `active` is `false` represent tracked structs that were created
    /// by a previous execution of the query, but not in the current one, and hence can
    /// be collected.
    active: bool,
}

// ANCHOR: ValueStruct
pub struct Value<C>
where
    C: Configuration,
{
    /// The revision when this tracked struct was last updated.
    /// This field also acts as a kind of "lock" over the `value` field. Once it is equal
    /// to `Some(current_revision)`, the fields are locked and
    /// cannot change further. This makes it safe to give out `&`-references
    /// so long as they do not live longer than the current revision
    /// (which is assured by tying their lifetime to the lifetime of an `&`-ref
    /// to the database).
    ///
    /// The struct is updated from an older revision `R0` to the current revision `R1`
    /// when the struct is first accessed in `R1`, whether that be because the original
    /// query re-created the struct (i.e., by user calling `Struct::new`) or because
    /// the struct was read from. (Structs may not be recreated in the new revision if
    /// the inputs to the query have not changed.)
    ///
    /// When re-creating the struct, the field is temporarily set to `None`.
    /// This is signal that there is an active `&mut` modifying the other fields:
    /// even reading from those fields in that situation would create UB.
    /// This `None` value should never be observable by users unless they have
    /// leaked a reference across threads somehow.
    updated_at: OptionalAtomicRevision,

    /// The durability minimum durability of all inputs consumed
    /// by the creator query prior to creating this tracked struct.
    /// If any of those inputs changes, then the creator query may
    /// create this struct with different values.
    durability: Durability,

    /// The revision information for each field: when did this field last change.
    /// When tracked structs are re-created, this revision may be updated to the
    /// current revision if the value is different.
    revisions: C::Revisions,

    /// Fields of this tracked struct. They can change across revisions,
    /// but they do not change within a particular revision.
    ///
    /// TODO: Consider whether we need a more explicit aliasing barrier or whether
    /// this should be restructured (e.g., with a nested struct for `fields` + `memos`)
    /// to make the aliasing guarantees more obvious. See PR #741 for prior discussion.
    fields: C::Fields<'static>,

    /// Memo table storing the results of query functions etc.
    /*unsafe */
    memos: MemoTable,
}

// ANCHOR_END: ValueStruct

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Copy, Clone)]
#[cfg_attr(feature = "persistence", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "persistence", serde(transparent))]
pub struct Disambiguator(u32);

#[derive(Default, Debug)]
pub(crate) struct DisambiguatorMap {
    // we use a non-hasher hashmap here as our key contains its own hash (in a sense)
    // so we use the raw entry api instead to avoid the overhead of hashing unnecessarily
    map: hashbrown::HashMap<IdentityHash, Disambiguator, ()>,
}

impl DisambiguatorMap {
    pub(crate) fn disambiguate(&mut self, key: IdentityHash) -> Disambiguator {
        use hashbrown::hash_map::RawEntryMut;

        let entry = self.map.raw_entry_mut().from_hash(key.hash, |k| *k == key);
        let disambiguator = match entry {
            RawEntryMut::Occupied(occupied) => occupied.into_mut(),
            RawEntryMut::Vacant(vacant) => {
                vacant
                    .insert_with_hasher(key.hash, key, Disambiguator(0), |k| k.hash)
                    .1
            }
        };
        let result = *disambiguator;
        disambiguator.0 += 1;
        result
    }

    pub(crate) fn seed<'a>(&mut self, identities: impl Iterator<Item = &'a Identity>) {
        for identity in identities {
            self.disambiguate(IdentityHash {
                ingredient_index: identity.ingredient_index,
                hash: identity.hash,
            });
        }
    }

    pub(crate) fn clear(&mut self) {
        self.map.clear()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl<C> IngredientImpl<C>
where
    C: Configuration,
{
    /// Create a tracked struct ingredient. Generated by the `#[tracked]` macro,
    /// not meant to be called directly by end-users.
    fn new(index: IngredientIndex) -> Self {
        Self {
            ingredient_index: index,
            phantom: PhantomData,
            free_list: Default::default(),
            // ryo-v2 fork: one shard: a tracked slot is taken again by any layer
            layer_free: LayerFreeList::new(1),
            memo_table_types: Arc::new(MemoTableTypes::default()),
        }
    }

    /// Returns the database key index for a tracked struct with the given id.
    pub fn database_key_index(&self, id: Id) -> DatabaseKeyIndex {
        DatabaseKeyIndex::new(self.ingredient_index, id)
    }

    pub fn new_struct<'db>(
        &'db self,
        // ryo-v2 fork: the view; under a layer the struct keeps its id and the layer holds its
        // new fields (`new_struct_in_layer`)
        zalsa: &'db ZalsaView,
        zalsa_local: &'db ZalsaLocal,
        mut fields: C::Fields<'db>,
    ) -> C::Struct<'db> {
        // ryo-v2 fork: under a layer, the layer's entry; without one, creating or reusing a struct
        // writes the base, refused while the base is frozen
        if let Some(layer) = zalsa.layer() {
            return self.new_struct_in_layer(zalsa, layer, zalsa_local, fields);
        }
        zalsa.fork().assert_not_frozen("a tracked-struct creation");
        let identity_hash = IdentityHash {
            ingredient_index: self.ingredient_index,
            hash: crate::hash::hash(&C::untracked_fields(&fields)),
        };

        let (current_deps, disambiguator) = zalsa_local.disambiguate(identity_hash);

        let identity = Identity {
            hash: identity_hash.hash,
            ingredient_index: identity_hash.ingredient_index,
            disambiguator,
        };

        if let Some(id) = zalsa_local.tracked_struct_id(&identity) {
            // The struct already exists in the intern map.
            crate::tracing::trace!(
                "Reuse tracked struct {id:?}",
                id = self.database_key_index(id)
            );

            // SAFETY: The `id` was present in the interned map, so the value must be initialized.
            let update_result = unsafe { self.update(zalsa, id, &current_deps, fields) };

            fields = match update_result {
                // Overwrite the previous ID if we are reusing the old slot with new fields.
                Ok(updated_id) if updated_id != id => {
                    zalsa_local.store_tracked_struct_id(identity, updated_id);
                    return FromId::from_id(updated_id);
                }

                // The id has not changed.
                Ok(id) => return FromId::from_id(id),

                // Failed to perform the update, we are forced to allocate a new slot.
                Err(fields) => fields,
            };
        }

        // We failed to perform the update, or this is a new tracked struct, so allocate a new entry
        // in the struct map.
        let id = self.allocate(zalsa, zalsa_local, &current_deps, fields);
        crate::tracing::trace!(
            "Allocated new tracked struct {key:?}",
            key = self.database_key_index(id)
        );
        zalsa_local.store_tracked_struct_id(identity, id);
        FromId::from_id(id)
    }

    // ryo-v2 fork: `new_struct` under a layer (D11, 4.2). A struct the creator made before (its
    // id is in the identity map the old memo seeded) keeps its id: the layer gets an entry with
    // the new fields, the per-field revisions and the durability, computed as `update` does on
    // the chain's current value; the base slot is not touched. A struct the creator makes anew,
    // or whose identity fields changed, gets a new slot only the layer reaches (a counted shared
    // write). The base's free list is never used (its ids keep their generation for every chain).
    fn new_struct_in_layer<'db>(
        &'db self,
        zalsa: &'db ZalsaView,
        layer: &'db Layer,
        zalsa_local: &'db ZalsaLocal,
        mut fields: C::Fields<'db>,
    ) -> C::Struct<'db> {
        let identity_hash = IdentityHash {
            ingredient_index: self.ingredient_index,
            hash: crate::hash::hash(&C::untracked_fields(&fields)),
        };

        let (current_deps, disambiguator) = zalsa_local.disambiguate(identity_hash);

        let identity = Identity {
            hash: identity_hash.hash,
            ingredient_index: identity_hash.ingredient_index,
            disambiguator,
        };

        if let Some(id) = zalsa_local.tracked_struct_id(&identity) {
            crate::tracing::trace!(
                "Reuse tracked struct {id:?} in a layer",
                id = self.database_key_index(id)
            );
            fields = match self.update_in_layer(zalsa, layer, id, &current_deps, fields) {
                Ok(id) => return FromId::from_id(id),
                Err(fields) => fields,
            };
        }

        let id = self.allocate_in_layer(zalsa, layer, zalsa_local, &current_deps, fields);
        crate::tracing::trace!(
            "Allocated new tracked struct {key:?} in a layer",
            key = self.database_key_index(id)
        );
        zalsa_local.store_tracked_struct_id(identity, id);
        FromId::from_id(id)
    }

    // ryo-v2 fork: the re-creation of `id` under `layer`. Upstream's early return for a struct
    // already updated in this revision applies to the layer's own entry only (and to a slot the
    // layer allocated): `&` references into it may be live. A struct the layer has no entry for
    // gets one, even after a read of it in this layer: the entry aliases nothing a reader holds.
    // `Err` (allocate a new slot) when the identity fields changed, as upstream's new-generation
    // case, or when the chain tombstoned `id`.
    fn update_in_layer<'db>(
        &'db self,
        zalsa: &'db ZalsaView,
        layer: &'db Layer,
        id: Id,
        current_deps: &Stamp,
        fields: C::Fields<'db>,
    ) -> Result<Id, C::Fields<'db>> {
        let tracked = C::TRACKED_FIELD_INDICES.len();
        let (old_fields, old_revisions, old_durability): (&C::Fields<'db>, Box<[Revision]>, _) =
            match layer.find_tracked(id) {
                ChainTracked::Entry { own: true, .. } => return Ok(id),
                ChainTracked::Tombstone => return Err(fields),
                ChainTracked::Entry { value, own: false } => {
                    let entry = downcast_entry::<C>(value);
                    (
                        restore_fields::<C>(&entry.fields),
                        entry.revisions.clone(),
                        entry.durability,
                    )
                }
                ChainTracked::None => {
                    let data = Self::data_raw(zalsa.table(), id);
                    // SAFETY: `updated_at` is never exclusively borrowed, so borrowing it is sound.
                    let updated_at = unsafe { (*data).updated_at.load() };
                    match updated_at {
                        None => panic!("two concurrent writers to {id:?}, should not be possible"),
                        // A slot this layer allocated, already at the layer's revision.
                        Some(r) if r == layer.revision() => return Ok(id),
                        Some(_) => {}
                    }
                    // SAFETY: The frozen base does not write the slot; `updated_at` is `Some`.
                    let value = unsafe { &*data };
                    (
                        restore_fields::<C>(&value.fields),
                        (0..tracked).map(|i| value.revisions[i].load()).collect(),
                        value.durability,
                    )
                }
            };

        let mut changed: smallvec::SmallVec<[bool; 8]> = smallvec::smallvec![false; tracked];
        if C::fields_changed(old_fields, &fields, &mut changed) {
            return Err(fields);
        }
        // As `update`: a changed field gets the creator's `changed_at`; when the creator's
        // durability is below the struct's, every tracked field does.
        let lowered = current_deps.durability < old_durability;
        let revisions: Box<[Revision]> = (0..tracked)
            .map(|i| {
                if lowered || changed[i] {
                    current_deps.changed_at
                } else {
                    old_revisions[i]
                }
            })
            .collect();
        layer.insert_tracked(
            id,
            Box::new(LayerEntry::<C> {
                // SAFETY: Guaranteed by `Configuration` and retained only in this entry.
                fields: unsafe {
                    std::mem::transmute::<C::Fields<'db>, C::Fields<'static>>(fields)
                },
                revisions,
                durability: current_deps.durability,
            }),
        );
        Ok(id)
    }

    // ryo-v2 fork: a slot under a layer, owned by the layer and given back when it drops; only
    // the layer's chain reaches it. A slot a dropped layer of this freeze gave back is taken
    // again with a bumped generation, as upstream's free list does (one of an earlier freeze is
    // leaked: the base fingerprint of this freeze may read it); else a new slot (a counted shared
    // write). The base's free list is never used (a frozen base's freed ids keep their
    // generation for every chain).
    fn allocate_in_layer<'db>(
        &'db self,
        zalsa: &'db ZalsaView,
        layer: &'db Layer,
        zalsa_local: &'db ZalsaLocal,
        current_deps: &Stamp,
        fields: C::Fields<'db>,
    ) -> Id {
        let current_revision = zalsa.current_revision();
        let value = |_| Value {
            updated_at: OptionalAtomicRevision::new(Some(current_revision)),
            durability: current_deps.durability,
            revisions: C::new_revisions(current_deps.changed_at),
            // SAFETY: Guaranteed by `Configuration` and retained only in this tracked value.
            fields: unsafe { std::mem::transmute::<C::Fields<'db>, C::Fields<'static>>(fields) },
            // SAFETY: We only ever access the memos of a value that we allocated through
            // our `MemoTableTypes`.
            memos: unsafe { MemoTable::new(self.memo_table_types()) },
        };

        while let Some(free) = self.layer_free.pop(0) {
            if free.epoch != layer.epoch() {
                layer.record_leaked_slot();
                continue;
            }
            let Some(id) = free.id.next_generation() else {
                layer.record_leaked_slot();
                continue;
            };
            // SAFETY: The slot was given back by a dropped layer of this freeze, which owned it:
            // only that layer's chain reached it, and every layer of the chain has dropped, so no
            // reference to it is alive; the base fingerprint of this freeze does not read it (it
            // was allocated after the freeze), and no other layer takes it (it was popped here).
            let data_raw = unsafe { &mut *Self::data_raw(zalsa.table(), id) };
            // As upstream's free-list reuse: the old contents are dropped here. A layer inserts no
            // memo into a slot's table, so the memo table is empty.
            *data_raw = value(id);
            layer.own_slot(&self.layer_free, 0, id, true);
            return id;
        }

        zalsa
            .fork()
            .record_shared_write(SharedWriteKind::SlotAllocation);
        let (id, _) = zalsa_local.allocate::<Value<C>>(zalsa, self.ingredient_index, value);
        layer.own_slot(&self.layer_free, 0, id, false);
        id
    }

    // ryo-v2 fork: the struct `id` as `layer`'s chain sees it, read without a lock and without a
    // write: a layer's entry, else the base slot after one load of its `updated_at`. A tombstoned
    // or deleted struct panics as upstream's read lock does.
    fn layer_view<'db>(
        &'db self,
        zalsa: &'db ZalsaView,
        layer: &'db Layer,
        id: Id,
    ) -> LayerView<'db, C> {
        match layer.find_tracked(id) {
            ChainTracked::Entry { value, .. } => LayerView::Entry(downcast_entry::<C>(value)),
            ChainTracked::Tombstone => panic!(
                "write lock taken; value leaked across threads or user functions not deterministic \
                 (ryo-v2 fork: `{id:?}` was removed in this layer)"
            ),
            ChainTracked::None => {
                let data = Self::data_raw(zalsa.table(), id);
                // SAFETY: `updated_at` is never exclusively borrowed, so borrowing it is sound.
                if unsafe { (*data).updated_at.load() }.is_none() {
                    panic!(
                        "write lock taken; value leaked across threads or user functions not deterministic"
                    );
                }
                LayerView::Base(data)
            }
        }
    }

    // ryo-v2 fork: the fields, the revision of tracked field `relative` (if any) and the
    // durability of a struct read under a layer
    fn layer_fields<'db>(
        &'db self,
        zalsa: &'db ZalsaView,
        layer: &'db Layer,
        id: Id,
        relative: Option<usize>,
    ) -> (&'db C::Fields<'db>, Revision, Durability) {
        match self.layer_view(zalsa, layer, id) {
            LayerView::Entry(entry) => (
                restore_fields::<C>(&entry.fields),
                relative.map_or(Revision::start(), |i| entry.revisions[i]),
                entry.durability,
            ),
            LayerView::Base(data) => {
                // SAFETY: `data` is a live slot of the frozen base with `updated_at` `Some`; the
                // frozen base does not write its fields or revisions.
                let value = unsafe { &*data };
                (
                    restore_fields::<C>(&value.fields),
                    relative.map_or(Revision::start(), |i| value.revisions[i].load()),
                    value.durability,
                )
            }
        }
    }

    // ryo-v2 fork: `remove_stale_output` under a layer: the struct is tombstoned in the layer
    // and the base slot stays. As upstream's `delete_entity`, the outputs of the memos keyed by
    // the struct (as the chain sees them) are removed too, recursively, in the layer.
    fn tombstone_in_layer(&self, zalsa: &ZalsaView, layer: &Layer, id: Id) {
        if let ChainTracked::Tombstone = layer.find_tracked(id) {
            return;
        }
        zalsa.event(&|| {
            Event::new(crate::EventKind::DidDiscard {
                key: self.database_key_index(id),
            })
        });
        let (functions, base_visible) = layer.memo_functions(id);
        let mut memos: Vec<(&dyn crate::table::memo::Memo, IngredientIndex)> = Vec::new();
        for function in &functions {
            if let ChainMemo::Found(memo) = layer.find_memo(*function, id) {
                memos.push((memo, *function));
            }
        }
        if base_visible {
            zalsa
                .table()
                .memos_in_layer(id)
                .for_each_memo(|memo_ingredient_index, memo| {
                    let function = zalsa
                        .ingredient_index_for_memo(self.ingredient_index, memo_ingredient_index);
                    if !functions.contains(&function) {
                        memos.push((memo, function));
                    }
                });
        }
        layer.tombstone(id);
        for (memo, function) in memos {
            let executor = DatabaseKeyIndex::new(function, id);
            zalsa.event(&|| Event::new(EventKind::DidDiscard { key: executor }));
            memo.remove_outputs(zalsa, executor);
        }
    }

    fn allocate<'db>(
        &'db self,
        // ryo-v2 fork: the view of a base handle (a layer allocates in `allocate_in_layer`)
        zalsa: &'db ZalsaView,
        zalsa_local: &'db ZalsaLocal,
        current_deps: &Stamp,
        fields: C::Fields<'db>,
    ) -> Id {
        // ryo-v2 fork: the handle's revision (a base handle's)
        let current_revision = zalsa.current_revision();
        let value = |_| Value {
            updated_at: OptionalAtomicRevision::new(Some(current_revision)),
            durability: current_deps.durability,
            revisions: C::new_revisions(current_deps.changed_at),

            // SAFETY: Guaranteed by `Configuration` and retained only in this tracked value.
            fields: unsafe { std::mem::transmute::<C::Fields<'db>, C::Fields<'static>>(fields) },
            // SAFETY: We only ever access the memos of a value that we allocated through
            // our `MemoTableTypes`.
            memos: unsafe { MemoTable::new(self.memo_table_types()) },
        };

        while let Some(id) = self.free_list.pop() {
            // Increment the ID generation before reusing it, as if we have allocated a new
            // slot in the table.
            //
            // If the generation would overflow, we are forced to leak the slot. Note that this
            // shouldn't be a problem in general as sufficient bits are reserved for the generation.
            let Some(id) = id.next_generation() else {
                crate::tracing::info!(
                    "leaking tracked struct {:?} due to generation overflow",
                    self.database_key_index(id)
                );

                continue;
            };

            // SAFETY: `data_raw` is a live-unaliased pointer
            let data_raw = unsafe { &mut *Self::data_raw(zalsa.table(), id) };
            debug_assert!(
                data_raw.updated_at.load().is_none(),
                "free list entry for `{id:?}` does not have `None` for `updated_at`"
            );

            // Overwrite the free-list entry. Use `*foo = ` because the entry
            // has been previously initialized and we want to free the old contents.
            *data_raw = value(id);
            return id;
        }

        let (id, _) = zalsa_local.allocate::<Value<C>>(zalsa, self.ingredient_index, value);

        id
    }

    /// Get mutable access to the data for `id` -- this holds a write lock for the duration
    /// of the returned value.
    ///
    /// # Panics
    ///
    /// * If the value is not present in the map.
    /// * If the value is already updated in this revision.
    ///
    /// # Safety
    ///
    /// The value at the given `id` must be initialized.
    unsafe fn update<'db>(
        &'db self,
        // ryo-v2 fork: the view of a base handle (a layer re-creates in `update_in_layer`)
        zalsa: &'db ZalsaView,
        mut id: Id,
        current_deps: &Stamp,
        fields: C::Fields<'db>,
    ) -> Result<Id, C::Fields<'db>> {
        // ryo-v2 fork: an update of a tracked struct is a base write, refused while the base is
        // frozen; a layer never reaches here
        zalsa.fork().assert_not_frozen("a tracked-struct update");
        let data_raw = Self::data_raw(zalsa.table(), id);

        // The protocol is:
        //
        // * When we begin updating, we store `None` in the `updated_at` field
        // * When completed, we store `Some(current_revision)` in `updated_at`
        //
        // No matter what mischief users get up to, it should be impossible for us to
        // observe `None` in `updated_at`. The `id` should only be associated with one
        // query and that query can only be running in one thread at a time.
        //
        // We *can* observe `Some(current_revision)` however, which means that this
        // tracked struct is already updated for this revision in two ways.
        // In that case we should not modify or touch it because there may be
        // `&`-references to its contents floating around.
        //
        // Observing `Some(current_revision)` can happen in two scenarios:
        // - leaks, see tests\preverify-struct-with-leaked-data.rs and tests\preverify-struct-with-leaked-data-2.rs
        // - or the following scenario (FIXME verify this? There is no test that covers this behavior):
        //
        // * Revision 1:
        //   * Tracked function F creates tracked struct S
        //   * F reads input I
        // * Revision 2: I is changed, F is re-executed
        //
        // When F is re-executed in rev 2, we first try to validate F's inputs/outputs,
        // which is the list [output: S, input: I]. As no inputs have changed by the time
        // we reach S, we mark it as verified. But then input I is seen to have changed,
        // and so we re-execute F. Note that we *know* that S will have the same value
        // (barring program bugs).
        //
        // Further complicating things: it is possible that F calls F2
        // and gives it (e.g.) S as one of its arguments. Validating F2 may cause F2 to
        // re-execute which means that it may indeed have read from S's fields
        // during the current revision and thus obtained an `&` reference to those fields
        // that is still live.

        {
            // SAFETY: `updated_at` is never exclusively borrowed, so borrowing it is sound
            let last_updated_at = unsafe { (*data_raw).updated_at.load() };
            assert!(
                last_updated_at.is_some(),
                "two concurrent writers to {id:?}, should not be possible"
            );

            // The value is already read-locked, but we can reuse it safely as per above.
            // ryo-v2 fork: the handle's revision (a base handle's)
            if last_updated_at == Some(zalsa.current_revision()) {
                return Ok(id);
            }

            // Updating the fields may make it necessary to increment the generation of the ID. In
            // the unlikely case that the ID is already at its maximum generation, we are forced to leak
            // the previous slot and allocate a new value.
            if id.generation() == u32::MAX {
                crate::tracing::info!(
                    "leaking tracked struct {:?} due to generation overflow",
                    self.database_key_index(id)
                );

                return Err(fields);
            }

            // Acquire the write-lock. This can only fail if there is a parallel thread
            // reading from this same `id`, which can only happen if the user has leaked it.
            // Tsk tsk.
            // SAFETY: `updated_at` is never exclusively borrowed, so borrowing it is sound
            let swapped = unsafe { (*data_raw).updated_at.swap(None) };
            if last_updated_at != swapped {
                panic!(
                    "failed to acquire write lock, id `{id:?}` \
                    must have been leaked across threads"
                );
            }
        }

        // SAFETY: We have claimed mutable access by swapping `None` into
        // `updated_at`, so the retained fields are exclusively borrowed.
        // `Configuration` guarantees that restoring the database lifetime is valid.
        let old_fields = unsafe {
            std::mem::transmute::<&mut C::Fields<'static>, &mut C::Fields<'_>>(
                &mut (*data_raw).fields,
            )
        };

        // SAFETY: `revisions` contains `AtomicRevision` values which can be safely accessed
        // concurrently with `tracked_field::maybe_changed_after`.
        let revisions = unsafe { &(*data_raw).revisions };

        let identity_fields_changed =
            C::update_fields(current_deps.changed_at, revisions, old_fields, fields);

        if identity_fields_changed {
            // Consider this a new tracked struct when any identity field changed.
            // This should be rare and only ever happen if there's a hash collision.
            //
            // Note that we hold the lock and have exclusive access to the tracked struct data,
            // so there should be no live instances of IDs from the previous generation. We clear
            // the memos and return a new ID here as if we have allocated a new slot.
            // SAFETY: We have acquired the write lock by swapping `None` into `updated_at`
            let memo_table = unsafe { &mut (*data_raw).memos };

            // SAFETY: The memo table belongs to a value that we allocated, so it has the
            // correct type.
            unsafe { self.clear_memos(zalsa, memo_table, id) };

            id = id
                .next_generation()
                .expect("already verified that generation is not maximum");
        }

        let durability = unsafe { &mut (*data_raw).durability };
        if current_deps.durability < *durability {
            let new_revisions = C::new_revisions(current_deps.changed_at);
            for i in 0..C::TRACKED_FIELD_INDICES.len() {
                revisions[i].store(new_revisions[i].load());
            }
        }
        *durability = current_deps.durability;
        // SAFETY: `updated_at` is never exclusively borrowed, so borrowing it is sound
        // release the lock
        // ryo-v2 fork: the handle's revision (a base handle's)
        let swapped_out = unsafe { (*data_raw).updated_at.swap(Some(zalsa.current_revision())) };
        assert!(
            swapped_out.is_none(),
            "two concurrent writers to {id:?}, should not be possible"
        );

        Ok(id)
    }

    /// Fetch the data for a given id created by this ingredient from the table,
    /// -giving it the appropriate type.
    fn data_raw(table: &Table, id: Id) -> *mut Value<C> {
        table.get_raw(id)
    }

    /// # Safety
    ///
    /// `data` must be a valid pointer to a `Value<C>`
    unsafe fn lock_fields(
        &self,
        data: *const Value<C>,
        current_revision: Revision,
    ) -> &C::Fields<'_> {
        // SAFETY: `data` is a valid pointer
        acquire_read_lock(unsafe { &(*data).updated_at }, current_revision);
        // SAFETY: `data` is valid, the read lock keeps its fields immutable,
        // and `Configuration` guarantees lifetime restoration is valid.
        unsafe { std::mem::transmute::<&C::Fields<'static>, &C::Fields<'_>>(&(*data).fields) }
    }

    /// Deletes the given entities. This is used after a query `Q` executes and we can compare
    /// the entities `E_now` that it produced in this revision vs the entities
    /// `E_prev` it produced in the last revision. Any missing entities `E_prev - E_new` can be
    /// deleted.
    ///
    /// # Warning
    ///
    /// Using this method on an entity id that MAY be used in the current revision will lead to
    /// unspecified results (but not UB). See [`InternedIngredient::delete_index`] for more
    /// discussion and important considerations.
    // ryo-v2 fork: the view of a base handle (a layer tombstones in `tombstone_in_layer`)
    pub(crate) fn delete_entity(&self, zalsa: &ZalsaView, id: Id) {
        // ryo-v2 fork: deleting a tracked struct is a base write, refused while the base is
        // frozen; a layer never reaches here
        zalsa.fork().assert_not_frozen("a tracked-struct deletion");
        zalsa.event(&|| {
            Event::new(crate::EventKind::DidDiscard {
                key: self.database_key_index(id),
            })
        });

        let data = Self::data_raw(zalsa.table(), id);

        // We want to set `updated_at` to `None`, signalling that other field values
        // cannot be read. The current value should be `Some(R0)` for some older revision.
        match unsafe { (*data).updated_at.swap(None) } {
            None => {
                panic!("cannot delete write-locked id `{id:?}`; value leaked across threads");
            }
            // ryo-v2 fork: the handle's revision (a base handle's)
            Some(r) if r == zalsa.current_revision() => panic!(
                "cannot delete read-locked id `{id:?}`; value leaked across threads or user functions not deterministic"
            ),
            Some(_) => (),
        }

        // SAFETY: We have acquired the write lock by swapping `None` into `updated_at`
        let memo_table = unsafe { &mut (*data).memos };

        // SAFETY: The memo table belongs to a value that we allocated, so it
        // has the correct type.
        unsafe { self.clear_memos(zalsa, memo_table, id) };

        // now that all cleanup has occurred, make available for re-use
        self.free_list.push(id);
    }

    /// Clears the given memo table.
    ///
    /// # Safety
    ///
    /// The `MemoTable` must belong to a `Value` of the correct type.
    // ryo-v2 fork: the view of a base handle (removing the memos' outputs takes it)
    pub(crate) unsafe fn clear_memos(&self, zalsa: &ZalsaView, memo_table: &mut MemoTable, id: Id) {
        // SAFETY: The caller guarantees this is the correct types table.
        let table = unsafe { self.memo_table_types.attach_memos_mut(memo_table) };

        // `Database::salsa_event` is a user supplied callback which may panic
        // in that case we need a drop guard to free the memo table
        struct TableDropGuard<'a>(MemoTableWithTypesMut<'a>);
        impl Drop for TableDropGuard<'_> {
            fn drop(&mut self) {
                // SAFETY: We have `&mut MemoTable`, so no more references to these memos exist and we are good
                // to drop them.
                unsafe { self.0.drop() };
            }
        }

        let mut table_guard = TableDropGuard(table);

        // SAFETY: We have `&mut MemoTable`, so no more references to these memos exist and we are good
        // to drop them.
        unsafe {
            table_guard.0.take_memos(|memo_ingredient_index, memo| {
                let ingredient_index =
                    zalsa.ingredient_index_for_memo(self.ingredient_index, memo_ingredient_index);

                let executor = DatabaseKeyIndex::new(ingredient_index, id);

                zalsa.event(&|| Event::new(EventKind::DidDiscard { key: executor }));

                memo.remove_outputs(zalsa, executor);
            })
        };

        mem::forget(table_guard);

        // Reset the table after having dropped any memos.
        memo_table.reset();
    }

    /// Return reference to the field data ignoring dependency tracking.
    /// Used for debugging.
    pub fn leak_fields<'db>(
        &'db self,
        // ryo-v2 fork: the view; under a layer the chain's entry, else the base slot unlocked
        zalsa: &'db ZalsaView,
        s: C::Struct<'db>,
    ) -> &'db C::Fields<'db> {
        let id = AsId::as_id(&s);
        // ryo-v2 fork: under a layer no read lock is taken (the base slot is not written)
        if let Some(layer) = zalsa.layer() {
            return self.layer_fields(zalsa, layer, id, None).0;
        }
        let data = Self::data_raw(zalsa.table(), id);
        // SAFETY: `data` is a valid pointer acquired from the table.
        // ryo-v2 fork: the handle's revision (a base handle's)
        unsafe { self.lock_fields(data, zalsa.current_revision()) }
    }

    /// Access to this tracked field.
    ///
    /// Note that this function returns the entire tuple of value fields.
    /// The caller is responsible for selecting the appropriate element.
    pub fn tracked_field<'db>(
        &'db self,
        // ryo-v2 fork: the view; under a layer the chain's entry, else the base slot unlocked
        zalsa: &'db ZalsaView,
        zalsa_local: &'db ZalsaLocal,
        s: C::Struct<'db>,
        relative_tracked_index: usize,
    ) -> &'db C::Fields<'db> {
        let id = AsId::as_id(&s);
        let field_ingredient_index = self.ingredient_index.successor(relative_tracked_index);
        // ryo-v2 fork: under a layer the fields, the field's revision and the durability are the
        // chain's entry's (else the base slot's), read without a lock
        if let Some(layer) = zalsa.layer() {
            let (fields, changed_at, durability) =
                self.layer_fields(zalsa, layer, id, Some(relative_tracked_index));
            zalsa_local.report_tracked_read_simple(
                DatabaseKeyIndex::new(field_ingredient_index, id),
                durability,
                changed_at,
            );
            return fields;
        }
        let data = Self::data_raw(zalsa.table(), id);

        // SAFETY: `data` is a valid pointer acquired from the table.
        // ryo-v2 fork: the handle's revision (a base handle's)
        let fields = unsafe { self.lock_fields(data, zalsa.current_revision()) };

        // SAFETY: We just acquired the read lock (in `Self::fields`), so accessing `revisions` will not be able to alias
        let field_changed_at = unsafe { (&(*data).revisions)[relative_tracked_index].load() };

        // SAFETY: We just acquired the read lock (in `Self::fields`), so accessing `durability` will not be able to alias
        zalsa_local.report_tracked_read_simple(
            DatabaseKeyIndex::new(field_ingredient_index, id),
            unsafe { (*data).durability },
            field_changed_at,
        );

        fields
    }

    /// Access to this untracked field.
    ///
    /// Note that this function returns the entire tuple of value fields.
    /// The caller is responsible for selecting the appropriate element.
    pub fn untracked_field<'db>(
        &'db self,
        // ryo-v2 fork: the view; under a layer the chain's entry, else the base slot unlocked
        zalsa: &'db ZalsaView,
        s: C::Struct<'db>,
    ) -> &'db C::Fields<'db> {
        let id = AsId::as_id(&s);
        // ryo-v2 fork: under a layer no read lock is taken (the base slot is not written)
        if let Some(layer) = zalsa.layer() {
            return self.layer_fields(zalsa, layer, id, None).0;
        }
        let data = Self::data_raw(zalsa.table(), id);

        // Note that we do not need to add a dependency on the tracked struct
        // as IDs that are reused increment their generation, invalidating any
        // dependent queries directly.
        // SAFETY: `data` is a valid pointer acquired from the table.
        // ryo-v2 fork: the handle's revision (a base handle's)
        unsafe { self.lock_fields(data, zalsa.current_revision()) }
    }

    /// Returns all data corresponding to the tracked struct.
    // ryo-v2 fork: the view; refused (panic) on a layered handle, whose structs are the base's
    // slots overlaid by the chain
    pub fn entries<'db>(
        &'db self,
        zalsa: &'db ZalsaView,
    ) -> impl Iterator<Item = StructEntry<'db, C>> {
        if zalsa.layer().is_some() {
            zalsa.fork().record_refusal();
            crate::fork::not_yet_supported("enumerating tracked structs");
        }
        self.entries_in_base(zalsa)
    }

    // ryo-v2 fork: the enumeration of a base, for `SalsaStructInDb::entries` (serialization and
    // memory usage, which need exclusive access and so no layered handle)
    #[doc(hidden)]
    pub fn entries_in_base<'db>(
        &'db self,
        zalsa: &'db Zalsa,
    ) -> impl Iterator<Item = StructEntry<'db, C>> {
        zalsa
            .table()
            .slots_of::<Value<C>>()
            .filter(|(_, value)| {
                // Deleted values remain allocated for reuse with `updated_at == None`.
                value.updated_at.load().is_some()
            })
            .map(|(id, value)| StructEntry {
                value,
                key: self.database_key_index(id),
            })
    }
}

/// A tracked struct entry.
pub struct StructEntry<'db, C>
where
    C: Configuration,
{
    #[cfg_attr(not(feature = "salsa_unstable"), allow(dead_code))]
    value: &'db Value<C>,
    key: DatabaseKeyIndex,
}

impl<'db, C> StructEntry<'db, C>
where
    C: Configuration,
{
    /// Returns the `DatabaseKeyIndex` for this entry.
    pub fn key(&self) -> DatabaseKeyIndex {
        self.key
    }

    /// Returns the tracked struct.
    pub fn as_struct(&self) -> C::Struct<'_> {
        FromId::from_id(self.key.key_index())
    }

    #[cfg(feature = "salsa_unstable")]
    pub fn value(&self) -> &'db Value<C> {
        self.value
    }
}

impl<C> Ingredient for IngredientImpl<C>
where
    C: Configuration,
{
    fn location(&self) -> &'static crate::ingredient::Location {
        &C::LOCATION
    }

    fn ingredient_index(&self) -> IngredientIndex {
        self.ingredient_index
    }

    unsafe fn maybe_changed_after(
        &self,
        // ryo-v2 fork: the view (the trait's)
        _zalsa: &ZalsaView,
        _db: crate::database::RawDatabase<'_>,
        _input: Id,
        _revision: Revision,
    ) -> VerifyResult {
        // Any change to a tracked struct results in a new ID generation, so there
        // are no direct dependencies on the struct, only on its tracked fields.
        panic!("nothing should ever depend on a tracked struct directly")
    }

    unsafe fn collect_minimum_serialized_edges(
        &self,
        _zalsa: &Zalsa,
        _edge: QueryEdge,
        _serialized_edges: &mut FxIndexSet<QueryEdge>,
        _visited_edges: &mut FxHashSet<QueryEdge>,
    ) {
        // Note that tracked structs are referenced by the identity map, but that
        // only matters if we are serializing the creating query, in which case
        // the dependency edge will be serialized directly.
        //
        // TODO: We could flatten the identity map here if the tracked struct is being
        // persisted, in order to more aggressively preserve the tracked struct IDs if
        // the transitive query is re-executed.
        panic!("nothing should ever depend on a tracked struct directly")
    }

    fn flatten_cycle_head_dependencies(
        &self,
        // ryo-v2 fork: the view (the trait's)
        _zalsa: &ZalsaView,
        _id: Id,
        _flattened_input_outputs: &mut FxIndexSet<QueryEdge>,
        _seen: &mut FxHashSet<DatabaseKeyIndex>,
    ) {
        panic!("nothing should ever depend on a tracked struct directly")
    }

    fn remove_stale_output(
        &self,
        // ryo-v2 fork: the view; under a layer the struct is tombstoned in the layer
        zalsa: &ZalsaView,
        _executor: DatabaseKeyIndex,
        stale_output_key: crate::Id,
    ) {
        // ryo-v2 fork: under a layer a tombstone; the base slot stays for every other chain
        if let Some(layer) = zalsa.layer() {
            self.tombstone_in_layer(zalsa, layer, stale_output_key);
            return;
        }
        // This method is called when, in prior revisions,
        // `executor` creates a tracked struct `salsa_output_key`,
        // but it did not in the current revision.
        // In that case, we can delete `stale_output_key` and any data associated with it.
        self.delete_entity(zalsa, stale_output_key)
    }

    fn debug_name(&self) -> &'static str {
        C::DEBUG_NAME
    }

    fn jar_kind(&self) -> JarKind {
        JarKind::Struct
    }

    fn memo_table_types(&self) -> &Arc<MemoTableTypes> {
        &self.memo_table_types
    }

    fn memo_table_types_mut(&mut self) -> &mut Arc<MemoTableTypes> {
        &mut self.memo_table_types
    }

    // ryo-v2 fork: at a thaw, the slots dropped layers gave back become the base's: each is
    // marked deleted (`updated_at` `None`, as `delete_entity` leaves a slot) and pushed onto the
    // base's own free list, from which a base allocation takes it again with a bumped generation
    fn fork_thaw(&mut self, table: &mut Table) {
        for free in self.layer_free.take_all() {
            // SAFETY: `table` is exclusive; the slot was given back by a dropped layer, so no
            // reference to it is alive.
            let data = unsafe { &mut *Self::data_raw(table, free.id) };
            data.updated_at.swap(None);
            self.free_list.push(free.id);
        }
    }

    /// Returns memory usage information about any tracked structs.
    #[cfg(feature = "salsa_unstable")]
    unsafe fn memory_usage(
        &self,
        db: &dyn crate::Database,
    ) -> Option<Vec<crate::database::SlotInfo>> {
        let memory_usage = self
            .entries(db.zalsa())
            // SAFETY: The memo table belongs to a value that we allocated, so it
            // has the correct type. The caller guarantees exclusive database access.
            .map(|entry| unsafe { entry.value.memory_usage(&self.memo_table_types) })
            .collect();

        Some(memory_usage)
    }

    fn is_persistable(&self) -> bool {
        C::PERSIST
    }

    fn should_serialize(&self, zalsa: &Zalsa) -> bool {
        C::PERSIST
            && zalsa
                .table()
                .slots_of::<Value<C>>()
                // Serialization has exclusive database access, so any value that is not
                // write-locked is live, even if it has not been verified in this revision.
                .any(|(_, value)| value.updated_at.load().is_some())
    }

    #[cfg(feature = "persistence")]
    unsafe fn serialize<'db>(
        &'db self,
        zalsa: &'db Zalsa,
        f: &mut dyn FnMut(&dyn erased_serde::Serialize),
    ) {
        f(&persistence::SerializeIngredient {
            zalsa,
            _ingredient: self,
        })
    }

    #[cfg(feature = "persistence")]
    fn deserialize(
        &mut self,
        zalsa: &mut Zalsa,
        deserializer: &mut dyn erased_serde::Deserializer,
    ) -> Result<(), erased_serde::Error> {
        let deserialize = persistence::DeserializeIngredient {
            zalsa,
            ingredient: self,
        };

        serde::de::DeserializeSeed::deserialize(deserialize, deserializer)
    }
}

impl<C> std::fmt::Debug for IngredientImpl<C>
where
    C: Configuration,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct(std::any::type_name::<Self>())
            .field("ingredient_index", &self.ingredient_index)
            .finish()
    }
}

impl<C> Value<C>
where
    C: Configuration,
{
    /// Fields of this tracked struct.
    ///
    /// They can change across revisions, but they do not change within
    /// a particular revision.
    #[cfg_attr(not(feature = "salsa_unstable"), doc(hidden))]
    pub fn fields(&self) -> &C::Fields<'_> {
        // SAFETY: Guaranteed by `Configuration`; the restored lifetime is
        // bounded by the borrow of this value.
        unsafe { std::mem::transmute::<&C::Fields<'static>, &C::Fields<'_>>(&self.fields) }
    }
}

#[inline]
fn acquire_read_lock(updated_at: &OptionalAtomicRevision, current_revision: Revision) {
    loop {
        match updated_at.load() {
            None => panic!(
                "write lock taken; value leaked across threads or user functions not deterministic"
            ),
            // the read lock was taken by someone else, so we also succeed
            Some(r) if r == current_revision => return,
            Some(r) => {
                if updated_at
                    .compare_exchange(Some(r), Some(current_revision))
                    .is_ok()
                {
                    break;
                }
            }
        }
    }
}

#[cfg(feature = "salsa_unstable")]
impl<C> Value<C>
where
    C: Configuration,
{
    /// Returns memory usage information about the tracked struct.
    ///
    /// # Safety
    ///
    /// The `MemoTable` must belong to a `Value` of the correct type. The caller must
    /// ensure exclusive access to the database's storage for the duration of this call.
    unsafe fn memory_usage(&self, memo_table_types: &MemoTableTypes) -> crate::database::SlotInfo {
        let heap_size = C::heap_size(self.fields());
        // SAFETY: The caller guarantees this is the correct types table.
        let memos = unsafe { memo_table_types.attach_memos(&self.memos) };

        crate::database::SlotInfo {
            debug_name: C::DEBUG_NAME,
            size_of_metadata: mem::size_of::<Self>() - mem::size_of::<C::Fields<'static>>(),
            size_of_fields: mem::size_of::<C::Fields<'static>>(),
            heap_size_of_fields: heap_size,
            // SAFETY: The caller guarantees exclusive access to the database's storage.
            memos: unsafe { memos.memory_usage() },
        }
    }
}

// SAFETY: `Value<C>` is our private type branded over the unique configuration `C`.
unsafe impl<C> Slot for Value<C>
where
    C: Configuration,
{
    #[inline(always)]
    unsafe fn memos(
        this: *const Self,
        current_revision: Revision,
    ) -> *const crate::table::memo::MemoTable {
        // Acquiring the read lock here with the current revision to ensure that there
        // is no danger of a race when deleting a tracked struct.
        // SAFETY: `this` is a valid pointer given the caller obligation
        unsafe { acquire_read_lock(&(*this).updated_at, current_revision) };
        // SAFETY: `this` is valid, and the read lock prevents slot reuse and deletion.
        unsafe { Self::memos_unchecked(this) }
    }

    #[inline(always)]
    unsafe fn memos_unchecked(this: *const Self) -> *const crate::table::memo::MemoTable {
        // SAFETY: The caller provides an initialized slot and prevents reuse and deletion.
        unsafe { &raw const (*this).memos }
    }

    #[inline(always)]
    fn memos_mut(&mut self) -> &mut crate::table::memo::MemoTable {
        &mut self.memos
    }

    // ryo-v2 fork: under a layer no read lock is taken on the base slot: one load of
    // `updated_at`, which must not be `None` (as `acquire_read_lock` requires); the frozen base
    // does not delete or reuse the slot
    unsafe fn memos_in_layer(this: *const Self) -> *const crate::table::memo::MemoTable {
        // SAFETY: `this` is a valid pointer given the caller obligation
        if unsafe { (*this).updated_at.load() }.is_none() {
            panic!(
                "write lock taken; value leaked across threads or user functions not deterministic"
            );
        }
        // SAFETY: `this` is valid; the frozen base neither deletes nor reuses the slot.
        unsafe { Self::memos_unchecked(this) }
    }

    // ryo-v2 fork: `updated_at`, the durability and every field revision (isolation check, I2)
    fn fork_fingerprint(&self, hasher: &mut dyn std::hash::Hasher) -> bool {
        hasher.write_usize(self.updated_at.load().map_or(0, Revision::as_usize));
        hasher.write_u8(self.durability.index() as u8);
        for i in 0..C::TRACKED_FIELD_INDICES.len() {
            hasher.write_usize(self.revisions[i].load().as_usize());
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disambiguate_map_works() {
        let mut d = DisambiguatorMap::default();
        // set up all 4 permutations of differing field values
        let h1 = IdentityHash {
            ingredient_index: IngredientIndex::new(0),
            hash: 0,
        };
        let h2 = IdentityHash {
            ingredient_index: IngredientIndex::new(1),
            hash: 0,
        };
        let h3 = IdentityHash {
            ingredient_index: IngredientIndex::new(0),
            hash: 1,
        };
        let h4 = IdentityHash {
            ingredient_index: IngredientIndex::new(1),
            hash: 1,
        };
        assert_eq!(d.disambiguate(h1), Disambiguator(0));
        assert_eq!(d.disambiguate(h1), Disambiguator(1));
        assert_eq!(d.disambiguate(h2), Disambiguator(0));
        assert_eq!(d.disambiguate(h2), Disambiguator(1));
        assert_eq!(d.disambiguate(h3), Disambiguator(0));
        assert_eq!(d.disambiguate(h3), Disambiguator(1));
        assert_eq!(d.disambiguate(h4), Disambiguator(0));
        assert_eq!(d.disambiguate(h4), Disambiguator(1));
    }

    #[test]
    fn identity_map_works() {
        let mut d = IdentityMap::default();
        // set up all 8 permutations of differing field values
        let i1 = Identity {
            ingredient_index: IngredientIndex::new(0),
            hash: 0,
            disambiguator: Disambiguator(0),
        };
        let i2 = Identity {
            ingredient_index: IngredientIndex::new(1),
            hash: 0,
            disambiguator: Disambiguator(0),
        };
        let i3 = Identity {
            ingredient_index: IngredientIndex::new(0),
            hash: 1,
            disambiguator: Disambiguator(0),
        };
        let i4 = Identity {
            ingredient_index: IngredientIndex::new(1),
            hash: 1,
            disambiguator: Disambiguator(0),
        };
        let i5 = Identity {
            ingredient_index: IngredientIndex::new(0),
            hash: 0,
            disambiguator: Disambiguator(1),
        };
        let i6 = Identity {
            ingredient_index: IngredientIndex::new(1),
            hash: 0,
            disambiguator: Disambiguator(1),
        };
        let i7 = Identity {
            ingredient_index: IngredientIndex::new(0),
            hash: 1,
            disambiguator: Disambiguator(1),
        };
        let i8 = Identity {
            ingredient_index: IngredientIndex::new(1),
            hash: 1,
            disambiguator: Disambiguator(1),
        };
        // SAFETY: We don't use the IDs within salsa internals so this is fine
        unsafe {
            assert_eq!(d.insert(i1, Id::from_index(0)), None);
            assert_eq!(d.insert(i2, Id::from_index(1)), None);
            assert_eq!(d.insert(i3, Id::from_index(2)), None);
            assert_eq!(d.insert(i4, Id::from_index(3)), None);
            assert_eq!(d.insert(i5, Id::from_index(4)), None);
            assert_eq!(d.insert(i6, Id::from_index(5)), None);
            assert_eq!(d.insert(i7, Id::from_index(6)), None);
            assert_eq!(d.insert(i8, Id::from_index(7)), None);

            assert_eq!(d.reuse(&i1), Some(Id::from_index(0)));
            assert_eq!(d.reuse(&i2), Some(Id::from_index(1)));
            assert_eq!(d.reuse(&i3), Some(Id::from_index(2)));
            assert_eq!(d.reuse(&i4), Some(Id::from_index(3)));
            assert_eq!(d.reuse(&i5), Some(Id::from_index(4)));
            assert_eq!(d.reuse(&i6), Some(Id::from_index(5)));
            assert_eq!(d.reuse(&i7), Some(Id::from_index(6)));
            assert_eq!(d.reuse(&i8), Some(Id::from_index(7)));
        };
    }
}

#[cfg(feature = "persistence")]
mod persistence {
    use std::fmt;

    use serde::ser::{SerializeMap, SerializeStruct};
    use serde::{Deserialize, de};

    use super::{Configuration, IngredientImpl, Value};
    use crate::plumbing::Ingredient;
    use crate::revision::OptionalAtomicRevision;
    use crate::table::memo::MemoTable;
    use crate::zalsa::Zalsa;
    use crate::{Durability, Id};

    pub struct SerializeIngredient<'db, C>
    where
        C: Configuration,
    {
        pub zalsa: &'db Zalsa,
        pub _ingredient: &'db IngredientImpl<C>,
    }

    impl<C> serde::Serialize for SerializeIngredient<'_, C>
    where
        C: Configuration,
    {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            let Self { zalsa, .. } = self;

            let count = zalsa
                .table()
                .slots_of::<Value<C>>()
                // Deleted values remain allocated for reuse with `updated_at == None`.
                .filter(|(_, value)| value.updated_at.load().is_some())
                .count();
            let mut map = serializer.serialize_map(Some(count))?;

            for (id, value) in zalsa
                .table()
                .slots_of::<Value<C>>()
                .filter(|(_, value)| value.updated_at.load().is_some())
            {
                map.serialize_entry(&id.as_bits(), value)?;
            }

            map.end()
        }
    }

    impl<C> serde::Serialize for Value<C>
    where
        C: Configuration,
    {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            let mut value = serializer.serialize_struct("Value", 4)?;

            let Value {
                durability,
                updated_at,
                revisions,
                fields,
                memos: _,
            } = self;

            value.serialize_field("durability", &durability)?;
            value.serialize_field("updated_at", &updated_at)?;
            value.serialize_field("revisions", &revisions)?;
            value.serialize_field("fields", &SerializeFields::<C>(fields))?;

            value.end()
        }
    }

    struct SerializeFields<'db, C: Configuration>(&'db C::Fields<'static>);

    impl<C> serde::Serialize for SerializeFields<'_, C>
    where
        C: Configuration,
    {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            // SAFETY: Guaranteed by `Configuration`; the restored lifetime is
            // bounded by the borrow of the retained fields.
            C::serialize(
                unsafe { std::mem::transmute::<&C::Fields<'static>, &C::Fields<'_>>(self.0) },
                serializer,
            )
        }
    }

    pub struct DeserializeIngredient<'db, C>
    where
        C: Configuration,
    {
        pub zalsa: &'db mut Zalsa,
        pub ingredient: &'db mut IngredientImpl<C>,
    }

    impl<'de, C> de::DeserializeSeed<'de> for DeserializeIngredient<'_, C>
    where
        C: Configuration,
    {
        type Value = ();

        fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_map(self)
        }
    }

    impl<'de, C> de::Visitor<'de> for DeserializeIngredient<'_, C>
    where
        C: Configuration,
    {
        type Value = ();

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a map")
        }

        fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
        where
            M: de::MapAccess<'de>,
        {
            let DeserializeIngredient { zalsa, ingredient } = self;

            while let Some((id, value)) = access.next_entry::<u64, DeserializeValue<C>>()? {
                let id = Id::from_bits(id);
                let (page_idx, _) = crate::table::split_id(id);

                let value = Value::<C> {
                    updated_at: value.updated_at,
                    durability: value.durability,
                    revisions: value.revisions,
                    fields: value.fields.0,
                    // SAFETY: We only ever access the memos of a value that we allocated through
                    // our `MemoTableTypes`.
                    memos: unsafe { MemoTable::new(ingredient.memo_table_types()) },
                };

                // Force initialize the relevant page.
                zalsa.table_mut().force_page::<Value<C>>(
                    page_idx,
                    ingredient.ingredient_index(),
                    ingredient.memo_table_types(),
                );

                // Initialize the slot.
                //
                // SAFETY: We have a mutable reference to the database.
                let (allocated_id, _) = unsafe {
                    zalsa
                        .table()
                        .page::<Value<C>>(page_idx)
                        .allocate(page_idx, |_| value)
                        .unwrap_or_else(|_| panic!("serialized an invalid `Id`: {id:?}"))
                };

                assert_eq!(
                    allocated_id, id,
                    "values are serialized in allocation order"
                );
            }

            Ok(())
        }
    }

    #[derive(Deserialize)]
    #[serde(rename = "Value")]
    pub struct DeserializeValue<C: Configuration> {
        durability: Durability,
        updated_at: OptionalAtomicRevision,
        revisions: C::Revisions,
        #[serde(bound = "C: Configuration")]
        fields: DeserializeFields<C>,
    }

    struct DeserializeFields<C: Configuration>(C::Fields<'static>);

    impl<'de, C> serde::Deserialize<'de> for DeserializeFields<C>
    where
        C: Configuration,
    {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            C::deserialize(deserializer)
                .map(DeserializeFields)
                .map_err(de::Error::custom)
        }
    }
}
