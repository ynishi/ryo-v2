//! Oracle harness of ryo-v2.
//!
//! The Spec is the output: the expected diagnostics of a case are what an
//! outer `cargo check` reported once (kept under `corpus/<ws>/expected/`), and
//! the harness compares the in-memory verifier's answer with them, error code
//! by error code. `cargo check` runs only when the expected files are
//! regenerated (`just oracle-refresh`), never in a test. The expected side is
//! `cargo check --workspace --all-targets`, what the CI gate sees; `history` measures the
//! verifier's false positives on real commits.
#![warn(missing_docs)]

pub mod breakage;
pub mod cargo_check;
pub mod case;
pub mod groups;
pub mod history;
pub mod materials;
pub mod planted;
pub mod search_recall;
