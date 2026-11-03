// ryo-v2 fork: the view of the database a handle sees, base plus its layer (D11). Not upstream salsa code.
use std::ops::Deref;

use super::Layer;
use crate::sync::Arc;
use crate::zalsa::Zalsa;
use crate::zalsa_local::ZalsaLocal;
use crate::{Durability, Revision};

/// What a database handle sees: the shared [`Zalsa`] and the handle's [`Layer`], if any.
///
/// [`ZalsaDatabase::zalsa`](crate::plumbing::ZalsaDatabase::zalsa) returns `&ZalsaView`. It
/// derefs to [`Zalsa`], so code written for `&Zalsa` takes it unchanged, while its own
/// [`current_revision`](ZalsaView::current_revision) and
/// [`last_changed_revision`](ZalsaView::last_changed_revision) read the layer. `Zalsa`'s own
/// revision accessors are crate-private, so a site that would read the base revision where the
/// handle's revision is meant does not compile.
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
    pub(crate) fn clone_for_handle(&self) -> Self {
        ZalsaView {
            zalsa_impl: Arc::clone(&self.zalsa_impl),
            layer: self.layer.clone(),
        }
    }

    /// The shared [`Zalsa`], for `Arc::get_mut` when this is the only handle.
    pub(crate) fn zalsa_arc_mut(&mut self) -> &mut Arc<Zalsa> {
        &mut self.zalsa_impl
    }

    pub(crate) fn set_layer(&mut self, layer: Arc<Layer>) {
        self.layer = Some(layer);
    }

    /// The handle's layer, if it carries one.
    #[inline]
    pub fn layer(&self) -> Option<&Arc<Layer>> {
        self.layer.as_ref()
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

    /// Unwinds if the handle's own token or the base's pending write asks for it, as
    /// [`Zalsa`] does (after its `WillCheckCancellation` event); under a layer also when the
    /// token of an ancestor layer was cancelled.
    #[inline]
    pub(crate) fn unwind_if_revision_cancelled(&self, zalsa_local: &ZalsaLocal) {
        self.zalsa_impl.unwind_if_revision_cancelled(zalsa_local);
        if let Some(layer) = &self.layer
            && layer.ancestor_cancelled()
            && !zalsa_local.is_cancellation_disabled()
        {
            zalsa_local.unwind_cancelled();
        }
    }
}
