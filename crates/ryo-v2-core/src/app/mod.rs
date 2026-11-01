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

/// The blind classes of the fake verifiers in the tests of this module: every class a mark names.
#[cfg(test)]
pub(crate) const ALWAYS_BLIND: [crate::domain::UnseenClass; 4] = [
    crate::domain::UnseenClass::Borrow,
    crate::domain::UnseenClass::BuildScript,
    crate::domain::UnseenClass::ProcMacroCrate,
    crate::domain::UnseenClass::Visibility,
];
