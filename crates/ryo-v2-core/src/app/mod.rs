//! The use cases: read what they need, decide with [`crate::domain`], record
//! the outcome.
//!
//! What a use case needs from the outside world is a port: a trait declared
//! here, by its consumer, and implemented in [`crate::infra`]. Code here calls
//! ports, never the file system, a process or the environment directly.

pub mod apply;
pub mod ports;
pub mod search;
pub mod verify;
pub mod write;

use crate::app::ports::{CheckError, Verifier};
use crate::domain::{Edit, Finding, Scope, Verdict};

/// The verdict of `edits` at `scope` against `baseline`: a check, as the one-level case of
/// `Verdict::of_level`. What the decision and the check before a write ask of the checker.
///
/// The caller takes `baseline` once per decision (`Verifier::baseline`) and passes it in.
///
/// # Contract
/// - C1: the verdict is `Verdict::of_check` of the check's findings (`Verdict::of_level` with
///   nothing below, nothing deleted and the check's files); a check failure is returned as is.
///   tested: app::write::tests::decide_and_write_paths_give_the_same_verdict
///   tested: ryo-v2-ra tests/stack.rs search_and_apply_give_the_same_verdict_on_a_chain
pub fn verdict_of<V: Verifier>(
    verifier: &V,
    session: &mut V::Session,
    edits: &[Edit],
    scope: Scope,
    baseline: &[Finding],
) -> Result<Verdict, CheckError> {
    let checked = verifier.check(session, edits, scope)?;
    Ok(Verdict::of_check(baseline, &checked))
}

/// The blind classes of the fake verifiers in the tests of this module: every class a mark names.
#[cfg(test)]
pub(crate) const ALWAYS_BLIND: [crate::domain::UnseenClass; 4] = [
    crate::domain::UnseenClass::Borrow,
    crate::domain::UnseenClass::BuildScript,
    crate::domain::UnseenClass::ProcMacroCrate,
    crate::domain::UnseenClass::Visibility,
];
