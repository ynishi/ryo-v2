//! Hand-written shapes of planted states that the expander did not restore before
//! (SEARCH_LEDGER "Generation"): a re-export under a comment, an item with a trailing comment,
//! and a call folded into the value of a struct-literal field.

pub mod plan;

// The widest slot a plan holds.
pub const WIDE: u32 = 8;

const LIMIT: u32 = 3; // slots per plan

pub struct Slot {
    pub width: u32,
    pub count: u32,
}

pub fn needed(v: &[u32]) -> u32 {
    v.iter().sum()
}

pub fn fit(width: u32, limit: u32) -> u32 {
    width.min(limit).min(WIDE)
}

pub fn pack(v: &[u32], limit: u32) -> u32 {
    let width = needed(v);
    let fitted = fit(width, limit);
    fitted + LIMIT
}

pub fn slot(v: &[u32]) -> Slot {
    let width = needed(v);
    Slot { width, count: 1 }
}
