use crate::Slot;

pub fn plan_one(s: &Slot) -> u32 {
    s.width * s.count
}

pub const WIDE: u32 = 8;
