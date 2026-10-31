//! Speculative refactoring engine for Rust: Blueprints explored and verified in-process on rust-analyzer, built once after commit
//!
//! The umbrella crate: the one name users depend on. The code lives in
//! `ryo-v2-core`; this crate re-exports it.
#![warn(missing_docs)]

pub use ryo_v2_core::*;
