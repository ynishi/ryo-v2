//! Hand-written shapes of planted states that the expander did not restore before
//! (SEARCH_LEDGER "Generation"): a documented item moved to another module file, a reference
//! folded into an argument, and a literal folded into an argument.

pub mod plan;

pub struct Slot {
    pub width: u32,
    pub count: u32,
}

pub struct Spec {
    pub label: String,
}

pub struct Job {
    pub spec: Spec,
}

/// Total of the slots, as a plan counts them.
fn tally(v: &[u32]) -> u32 {
    v.iter().sum()
}

pub fn count_all(v: &[u32]) -> u32 {
    tally(v) + 1
}

pub fn measure(label: &str, limit: usize) -> Option<usize> {
    if label.len() <= limit {
        Some(label.len())
    } else {
        None
    }
}

pub fn scan(job: &Job, limit: usize) -> Option<usize> {
    let m = measure(&job.spec.label, limit)?;
    Some(m + 1)
}

pub fn fill(n: u32, per_row: u32) -> u32 {
    n * per_row
}

pub fn build(n: u32) -> u32 {
    let per_row = 20;
    let r = fill(n, per_row);
    r + 1
}
