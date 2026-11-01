//! The live fork of a session: its ticket ids, and the guards of the operations that need the
//! session at its base state.

use crate::session::{ForkPort, Live, RaSession};
use ryo_v2_core::app::ports::CheckError;
use std::sync::atomic::{AtomicU64, Ordering};

/// Ids of the tickets of every session of the process. A ticket of another session, or of a fork
/// that has ended, never equals the id of a live fork, which a counter per session could not
/// promise.
static NEXT_FORK_ID: AtomicU64 = AtomicU64::new(1);

/// Makes a new fork of `port` the live fork of `s` and returns the id of its ticket. The caller
/// has checked that no fork is live.
pub(crate) fn open_fork(s: &mut RaSession, port: ForkPort) -> u64 {
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
