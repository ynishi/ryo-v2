use crate::Slot;

pub fn plan_one(s: &Slot) -> u32 {
    s.width * s.count
}

/// Total of the slots, as a plan counts them.
pub(crate) fn tally(v: &[u32]) -> u32 {
    v.iter().sum()
}
