use crate::active_query::CompletedQuery;
use crate::function::memo::MemoHeader;
use crate::hash::FxIndexSet;
// ryo-v2 fork: stale outputs are removed through the handle's view
use crate::fork::ZalsaView;
use crate::zalsa_local::{QueryOriginRef, output_edges};
use crate::{DatabaseKeyIndex, Event, EventKind};

impl MemoHeader {
    /// Compute the old and new outputs and invoke `remove_stale_output` for each output that
    /// was generated before but is not generated now.
    pub(super) fn diff_outputs(
        &self,
        // ryo-v2 fork: the view; under a layer stale outputs are tombstoned in the layer
        zalsa: &ZalsaView,
        key: DatabaseKeyIndex,
        completed_query: &CompletedQuery,
    ) {
        let (QueryOriginRef::Derived(edges) | QueryOriginRef::DerivedUntracked(edges)) =
            self.origin()
        else {
            return;
        };

        // Note that tracked structs are not stored as direct query outputs, but they are still outputs
        // that need to be reported as stale.
        for (identity, id) in &completed_query.stale_tracked_structs {
            let output = DatabaseKeyIndex::new(identity.ingredient_index(), *id);
            report_stale_output(zalsa, key, output);
        }

        let mut stale_outputs = output_edges(edges).collect::<FxIndexSet<_>>();

        if stale_outputs.is_empty() {
            return;
        }

        // Preserve any outputs that were recreated in the current revision.
        for new_output in completed_query.revisions.origin().outputs() {
            stale_outputs.swap_remove(&new_output);
        }

        // Any outputs that were created in a previous revision but not the current one are stale.
        for output in stale_outputs {
            report_stale_output(zalsa, key, output);
        }
    }
}

// ryo-v2 fork: the view; under a layer the output is tombstoned in the layer
fn report_stale_output(zalsa: &ZalsaView, key: DatabaseKeyIndex, output: DatabaseKeyIndex) {
    zalsa.event(&|| {
        Event::new(EventKind::WillDiscardStaleOutput {
            execute_key: key,
            output_key: output,
        })
    });

    output.remove_stale_output(zalsa, key);
}
