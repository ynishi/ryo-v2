//! The live fork of a session: its ticket ids, the guard of the operations that need the session
//! at its base state, and the one write of a check.

use crate::RaAnalyzer;
use crate::check::map_error;
use crate::fork;
use crate::session::{ForkPort, Live, RaSession};
use ryo_v2_core::app::ports::{AnalyzerError, CheckError};
use ryo_v2_core::domain::Edit;
use std::sync::atomic::{AtomicU64, Ordering};

/// Ids of the tickets of every session of the process. A ticket of another session, or of a fork
/// that has ended, never equals the id of a live fork, which a counter per session could not
/// promise.
static NEXT_FORK_ID: AtomicU64 = AtomicU64::new(1);

/// Makes a new fork of `port` the live fork of `s` and returns the id of its ticket. The caller
/// has checked that no fork is live. The fork's level is a layer: the base is frozen here and
/// thawed when the fork ends; a base that cannot be frozen (another handle of the database is
/// alive) is `ForkInUse` and opens no fork.
pub(crate) fn open_fork(s: &mut RaSession, port: ForkPort) -> Result<u64, AnalyzerError> {
    crate::engine::freeze(s)?;
    Ok(mark_live(s, port))
}

/// Makes a new fork of `port` the live fork of `s`, whose base the caller has frozen
/// (`Forks::open`), and returns the id of its ticket.
pub(crate) fn mark_live(s: &mut RaSession, port: ForkPort) -> u64 {
    let id = NEXT_FORK_ID.fetch_add(1, Ordering::Relaxed);
    s.live = Some(Live { id, port });
    id
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

/// The one write of a check: counts it, records its paths for the revert audit (a refused write
/// too, so the sampling of writes does not move), and pushes its level. A write that panicked
/// the database has poisoned the session and is `Poisoned`.
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
    crate::engine::push_level(a, s, edits, edited.clone(), sampled).map_err(|e| match &s.poisoned {
        Some(msg) => CheckError::Poisoned(msg.clone()),
        None => map_error(e, &edited),
    })
}
