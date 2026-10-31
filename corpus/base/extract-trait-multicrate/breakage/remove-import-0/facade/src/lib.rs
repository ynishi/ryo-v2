//! CrossCrateExtractTrait fixture — facade side (P3 guard axis).
//!
//! Re-exports lib's `Case06` so that `viewer` can consume the type
//! WITHOUT declaring a direct dependency on the lib crate. The P3
//! suggester guard must wholesale-skip Case06: injecting
//! `use extract_trait_multicrate_lib::Case06Trait;` into viewer would
//! not resolve there (lib is not in viewer's `[dependencies]`).

