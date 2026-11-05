//! The live fork of a session: its ticket ids, the guards of the operations that need the session
//! at its base state, and `Stacked` for the adapter.

use crate::RaAnalyzer;
use crate::check::{diagnose_for_check, map_error, revert_error};
use crate::fork;
use crate::session::{ForkPort, Live, RaSession};
use ryo_v2_core::app::ports::{AnalyzerError, CheckError, Stacked};
use ryo_v2_core::domain::{Edit, RelPath, Scope, ScopedFindings};
use std::sync::atomic::{AtomicU64, Ordering};

/// Ids of the tickets of every session of the process. A ticket of another session, or of a fork
/// that has ended, never equals the id of a live fork, which a counter per session could not
/// promise.
static NEXT_FORK_ID: AtomicU64 = AtomicU64::new(1);

/// The ticket of the live stacked fork of a session. It is an id and nothing else: the session
/// holds the fork's levels, and a ticket that is not the live fork's (another session's, or one
/// already released) is refused with `ForkInUse`.
#[derive(Debug, Clone)]
pub struct RaStack {
    id: u64,
}

/// Makes a new fork of `port` the live fork of `s` and returns the id of its ticket. The caller
/// has checked that no fork is live. With `on_engine` the fork's levels are layers: the base is
/// frozen here and thawed when the fork ends; a base that cannot be frozen (another handle of the
/// database is alive) is `ForkInUse` and opens no fork.
pub(crate) fn open_fork(
    s: &mut RaSession,
    port: ForkPort,
    on_engine: bool,
) -> Result<u64, AnalyzerError> {
    if on_engine {
        crate::engine::freeze(s)?;
    }
    let id = NEXT_FORK_ID.fetch_add(1, Ordering::Relaxed);
    s.live = Some(Live { id, port });
    Ok(id)
}

/// Whether `id` is the ticket of the live fork, opened by `port`.
pub(crate) fn is_live(s: &RaSession, id: u64, port: ForkPort) -> bool {
    s.live.is_some_and(|l| l.id == id && l.port == port)
}

/// The guard of an operation that needs the session at its base state and holds no ticket:
/// the poison first, then a live fork.
pub(crate) fn require_free(s: &RaSession) -> Result<(), CheckError> {
    if let Some(msg) = &s.poisoned {
        return Err(CheckError::Poisoned(msg.clone()));
    }
    if s.live.is_some() {
        return Err(CheckError::ForkInUse);
    }
    Ok(())
}

/// What an expansion reads, by the fork live on its session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExpandOn {
    /// No fork is live: the state is written on an `Analyzer` fork of the base and read there.
    Base,
    /// A stacked fork is live: its top level is read, and nothing is written.
    StackedTop,
}

/// The guard of an expansion: the poison first; then a live `Analyzer` fork is `ForkInUse`, and a
/// live stacked fork is read at its top level. Forks that are open are `ForkInUse` too (`Forks`
/// C6): the base is frozen and `expand` takes no Layer to read (stage 4 reads one in a pool read
/// job).
pub(crate) fn expand_on(s: &RaSession) -> Result<ExpandOn, CheckError> {
    if let Some(msg) = &s.poisoned {
        return Err(CheckError::Poisoned(msg.clone()));
    }
    match s.live.map(|l| l.port) {
        None => Ok(ExpandOn::Base),
        Some(ForkPort::Stacked) => Ok(ExpandOn::StackedTop),
        Some(ForkPort::Analyzer | ForkPort::Forks) => Err(CheckError::ForkInUse),
    }
}

/// The guard of an operation that holds a ticket and may end or read the fork: the ticket against
/// the live fork. A wrong ticket changes nothing and wins over the poison.
fn require_ticket(s: &RaSession, f: &RaStack) -> Result<(), CheckError> {
    if is_live(s, f.id, ForkPort::Stacked) {
        Ok(())
    } else {
        Err(CheckError::ForkInUse)
    }
}

/// The guard of an operation that reads or writes the database through a ticket: the ticket,
/// then the poison.
fn require_usable(s: &RaSession, f: &RaStack) -> Result<(), CheckError> {
    require_ticket(s, f)?;
    match &s.poisoned {
        Some(msg) => Err(CheckError::Poisoned(msg.clone())),
        None => Ok(()),
    }
}

/// One write of a check or of `Stacked::write`: counts it, records its paths for the revert audit
/// (a refused write too, so the sampling of writes does not move), and pushes its level. A write
/// that panicked the database has poisoned the session and is `Poisoned`.
pub(crate) fn write_level(
    a: &RaAnalyzer,
    s: &mut RaSession,
    edits: &[Edit],
) -> Result<(), CheckError> {
    let edited = fork::edited_paths(edits);
    s.edited_since_audit.extend(edited.iter().cloned());

    s.write_count = s.write_count.wrapping_add(1);
    let sampled = s.assert_revert
        || a.config
            .assert_revert_every
            .is_some_and(|n| s.write_count.is_multiple_of(n.get()));
    fork::push_level(a, s, edits, edited.clone(), sampled).map_err(|e| match &s.poisoned {
        Some(msg) => CheckError::Poisoned(msg.clone()),
        None => map_error(e, &edited),
    })
}

/// The stacked writes of the adapter. On the undo log the levels are the session's undo log of
/// its one database; on the fork Engine (`RaConfig.engine`, D11, the default) a level is a layer
/// over the frozen base, `pop` drops the top layer and `release` drops them all, and a level's
/// write is checked by the isolation of the base (I2) instead of a revert. The cut-off (D1), the
/// detector-panic rule (D7) and the unseen classes of a diagnosis are the same on both.
///
/// # Contract
/// - C1: the diagnostic re-assert of a level runs when its write is sampled (`assert_revert`, or
///   every `assert_revert_every`-th write, a check being one write) and compares the scope of the
///   level's edits at the level below with the findings that scope had before the write (the
///   baseline for the first level).
///   tested: tests/stack.rs pop_reassert_compares_with_the_level_below
///   tested: tests/stack.rs writes_count_for_the_sampled_reassert
/// - C2: every write, a refused one included, is recorded for `RevertAudit::audit_revert`.
///   tested: tests/stack.rs stacked_writes_are_audited
/// - C3: an `Analyzer` fork and a stacked fork share the one live fork of the session.
///   tested: tests/stack.rs session_operations_refuse_while_a_fork_is_live
/// - C4: every unseen class of a diagnosis, a check's included, has in `unseen_at` each file it
///   was observed on and no other: the files where the unresolved-path detector was skipped for
///   a macro call that did not expand, and those where a detector panicked as it did at the
///   baseline; no class has an empty set.
///   tested: tests/stack.rs a_diagnosis_names_where_it_observed_each_unseen_class
/// - C5: a base text that `files_of` needs and cannot read (a database read that fails)
///   makes it `CheckError::Load`, of reach `Session`; the session is not poisoned, and the
///   fork, its levels and every text are as before.
///   tested: tests/stack.rs a_failed_base_read_is_a_session_error
/// - C6: every finding of a diagnosis, a check's included, is on a file the diagnosis covers.
///   An error is on the scanned file, or, when rust-analyzer reports its range in another file
///   (code pulled in with `include!`), on that file, which is in the scanned file's crate
///   (`diagnostics::collect_errors` C1, C3 and C4); and every scope holds whole crates.
///   tested: tests/stack.rs a_finding_is_on_a_file_of_its_diagnosis
/// - C7: on the Engine a level is sealed when a write pushes the next level on it, and stays
///   sealed until it is popped; a diagnosis of a sealed top level runs in a throwaway child of its
///   layer and gives what the level gave before it was sealed, and a level written on it is its
///   flat composition.
///   tested: tests/stack.rs a_sealed_level_diagnoses_through_a_read_child
/// - C8: on the Engine a panic inside a level's layer poisons that layer only. A write that
///   panics pushes no level and is a panic of reach Blueprint; a level whose diagnosis panicked
///   while its layer was open refuses every further diagnosis and every write on it the same
///   way, until it is popped. The levels below, a level written after the pop and the session
///   are untouched, and no write of the base was refused.
///   tested: tests/stack.rs a_panic_in_a_layer_poisons_only_that_layer
/// - C9: on the Engine, while a layer of the base lives, every operation that writes the base (a
///   check, the reference check, the revert audit, the memory report, a new fork) is refused as
///   a live fork is, at once, and changes nothing.
///   tested: tests/stack.rs a_base_write_with_a_live_layer_is_fork_in_use
impl Stacked for RaAnalyzer {
    type Fork = RaStack;

    fn fork(&self, s: &mut RaSession) -> Result<RaStack, CheckError> {
        require_free(s)?;
        let engine = s.engine;
        Ok(RaStack {
            id: open_fork(s, ForkPort::Stacked, engine).map_err(|e| map_error(e, &[]))?,
        })
    }

    fn write(&self, s: &mut RaSession, f: &mut RaStack, edits: &[Edit]) -> Result<(), CheckError> {
        require_usable(s, f)?;
        write_level(self, s, edits)
    }

    fn levels(&self, s: &RaSession, f: &RaStack) -> Result<usize, CheckError> {
        require_ticket(s, f)?;
        Ok(s.stack.len())
    }

    fn fork_text(
        &self,
        s: &RaSession,
        f: &RaStack,
        path: &RelPath,
    ) -> Result<Option<String>, CheckError> {
        require_usable(s, f)?;
        fork::text_at_top(s, path).map_err(|e| map_error(e, &[]))
    }

    fn diagnose(
        &self,
        s: &RaSession,
        f: &RaStack,
        scope: Scope,
    ) -> Result<ScopedFindings, CheckError> {
        require_usable(s, f)?;
        diagnose_for_check(self, s, scope)
    }

    fn files_of(
        &self,
        s: &RaSession,
        f: &RaStack,
        scope: Scope,
    ) -> Result<Vec<RelPath>, CheckError> {
        require_usable(s, f)?;
        // Only a read can fail here, and it writes nothing: a session error, not a poison.
        fork::files_of(self, s, scope)
            .map_err(|e| CheckError::Load(format!("database read failed: {e}")))
    }

    fn pop(&self, s: &mut RaSession, f: &mut RaStack) -> Result<bool, CheckError> {
        require_usable(s, f)?;
        fork::pop_level(self, s).map_err(revert_error)
    }

    fn release(&self, s: &mut RaSession, f: RaStack) -> Result<(), CheckError> {
        require_ticket(s, &f)?;
        fork::release_levels(self, s).map_err(revert_error)
    }
}
