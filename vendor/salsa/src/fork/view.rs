// ryo-v2 fork: the view of the database a handle sees, base plus its layer (D11). Not upstream salsa code.
use std::ops::Deref;

use super::Layer;
use crate::runtime::DependencyGraph;
use crate::salsa_struct::SalsaStructInDb;
use crate::sync::{Arc, Mutex};
use crate::table::memo::MemoTableWithTypes;
use crate::zalsa::Zalsa;
use crate::zalsa_local::ZalsaLocal;
use crate::{Durability, Id, Revision};

/// What a database handle sees: the shared [`Zalsa`] and the handle's [`Layer`], if any.
///
/// [`ZalsaDatabase::zalsa`](crate::plumbing::ZalsaDatabase::zalsa) returns `&ZalsaView`. It
/// derefs to [`Zalsa`], so code written for `&Zalsa` takes it unchanged, while its own
/// [`current_revision`](ZalsaView::current_revision) and
/// [`last_changed_revision`](ZalsaView::last_changed_revision) read the layer. `Zalsa`'s own
/// revision accessors are crate-private, and every salsa method that reads or writes a memo, a
/// stamp, a tracked struct or an interned value takes the view, so a site that would read the
/// base where the handle's layer is meant does not compile.
pub struct ZalsaView {
    zalsa_impl: Arc<Zalsa>,
    layer: Option<Arc<Layer>>,
}

impl Deref for ZalsaView {
    type Target = Zalsa;

    #[inline(always)]
    fn deref(&self) -> &Zalsa {
        &self.zalsa_impl
    }
}

/// A view with a layer was counted as a handle of it ([`ZalsaView::set_layer`],
/// [`ZalsaView::clone_for_handle`]): [`Layer::seal`] refuses while one lives.
impl Drop for ZalsaView {
    fn drop(&mut self) {
        if let Some(layer) = &self.layer {
            layer.remove_handle();
        }
    }
}

impl ZalsaView {
    pub(crate) fn new(zalsa: Zalsa) -> Self {
        ZalsaView {
            zalsa_impl: Arc::new(zalsa),
            layer: None,
        }
    }

    /// A copy of the view for a clone of its handle (`StorageHandle::clone`), keeping the layer.
    /// Not `Clone`: a copy outside a handle would hold the `Arc<Zalsa>` uncounted by the handle
    /// count, so `zalsa_mut` and thaw could not take it back.
    ///
    /// A copy with a layer counts as one more handle of the layer ([`Layer::handles`]); a base
    /// handle's copy touches no layer counter, so its cost is upstream's.
    pub(crate) fn clone_for_handle(&self) -> Self {
        if let Some(layer) = &self.layer {
            layer.add_handle();
        }
        ZalsaView {
            zalsa_impl: Arc::clone(&self.zalsa_impl),
            layer: self.layer.clone(),
        }
    }

    /// The shared [`Zalsa`], for `Arc::get_mut` when this is the only handle.
    pub(crate) fn zalsa_arc_mut(&mut self) -> &mut Arc<Zalsa> {
        &mut self.zalsa_impl
    }

    /// Sets `layer` on a view that has none; the view counts as a handle of it until it drops.
    pub(crate) fn set_layer(&mut self, layer: Arc<Layer>) {
        debug_assert!(self.layer.is_none());
        layer.add_handle();
        self.layer = Some(layer);
    }

    /// The handle's layer, if it carries one.
    #[inline]
    pub fn layer(&self) -> Option<&Arc<Layer>> {
        self.layer.as_ref()
    }

    /// The key of the handle's claims and blocked threads: the layer's, or 0 for a base handle.
    #[inline]
    pub(crate) fn layer_key(&self) -> usize {
        match &self.layer {
            Some(layer) => layer.key(),
            None => 0,
        }
    }

    /// The dependency graph the handle's queries block and transfer in: the layer's, or the
    /// base runtime's.
    #[inline]
    pub(crate) fn dependency_graph(&self) -> &Mutex<DependencyGraph> {
        match &self.layer {
            Some(layer) => layer.dependency_graph(),
            None => self.zalsa_impl.runtime().dependency_graph(),
        }
    }

    /// The handle's current revision: the layer's own revision, else the base's.
    ///
    /// **NOT SEMVER STABLE**
    #[doc(hidden)]
    #[inline]
    pub fn current_revision(&self) -> Revision {
        match &self.layer {
            Some(layer) => layer.revision(),
            None => self.zalsa_impl.base_revision(),
        }
    }

    /// The revision in which values of durability `durability` last changed, as the handle sees
    /// it. Under a layer: for `LOW`, the revision of the newest layer of the chain that has
    /// writes (a write-free layer inherits its parent's), else the base's; for higher
    /// durabilities the base's, since a layer writes only `LOW` fields.
    ///
    /// **NOT SEMVER STABLE**
    #[doc(hidden)]
    #[inline]
    pub fn last_changed_revision(&self, durability: Durability) -> Revision {
        let base = self.zalsa_impl.base_last_changed(durability);
        match &self.layer {
            Some(layer) if durability == Durability::LOW => layer.last_written().unwrap_or(base),
            _ => base,
        }
    }

    /// Returns the [`MemoTable`](crate::table::memo::MemoTable) of the base slot of the salsa
    /// struct `id`. Without a layer as upstream (which may take a tracked struct's read lock).
    /// Under a layer no lock is taken: a tracked struct's `updated_at` is loaded once and must not
    /// be `None`, which the frozen base cannot change.
    pub(crate) fn memo_table_for<T: SalsaStructInDb>(&self, id: Id) -> MemoTableWithTypes<'_> {
        match &self.layer {
            // SAFETY: We are supplying the correct current revision.
            None => unsafe { T::memo_table(self, id, self.zalsa_impl.base_revision()) },
            Some(_) => self.table().memos_in_layer(id),
        }
    }

    /// Unwinds if the handle's own token or the base's pending write asks for it, as
    /// [`Zalsa`] does (after its `WillCheckCancellation` event); under a layer also when the
    /// token of an ancestor layer was cancelled.
    #[inline]
    pub(crate) fn unwind_if_revision_cancelled(&self, zalsa_local: &ZalsaLocal) {
        self.zalsa_impl.unwind_if_revision_cancelled(zalsa_local);
        if self.ancestor_cancelled(zalsa_local) {
            zalsa_local.unwind_cancelled();
        }
    }

    /// Whether a query of this handle stops as cancelled: its own token (as upstream) or, under
    /// a layer, an ancestor's token, unless the handle disabled cancellation.
    #[inline]
    pub(crate) fn should_trigger_local_cancellation(&self, zalsa_local: &ZalsaLocal) -> bool {
        zalsa_local.should_trigger_local_cancellation() || self.ancestor_cancelled(zalsa_local)
    }

    #[inline]
    fn ancestor_cancelled(&self, zalsa_local: &ZalsaLocal) -> bool {
        match &self.layer {
            Some(layer) => !zalsa_local.is_cancellation_disabled() && layer.ancestor_cancelled(),
            None => false,
        }
    }
}
