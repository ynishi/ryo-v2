//! breakage-lab `lab-shapes`: traits, generic bounds, consts, a cfg-gated item.
//! `// @brk <kind>: <code>` lines are anchors for `breakage gen`.

use std::fmt;

pub trait Area {
    fn area(&self) -> u32;
}

pub trait Named {
    fn name(&self) -> String;
}

pub struct Square {
    pub side: u32,
}

pub struct Rect {
    pub w: u32,
    pub h: u32,
}

impl Area for Square {
    fn area(&self) -> u32 {
        self.side * self.side
    }
}

impl Area for Rect {
    fn area(&self) -> u32 {
        self.w * self.h
    }
}

impl Named for Square {
    fn name(&self) -> String {
        String::from("square")
    }
}

// impl fmt::Display for Rect {
//     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
//         write!(f, "{}x{}", self.w, self.h)
//     }
// }

pub fn total_area<T: Area>(items: &[T]) -> u32 {
    items.iter().map(|i| i.area()).sum()
}

pub fn describe<T: fmt::Display>(item: &T) -> String {
    format!("<{}>", item)
}

pub fn label<T: Named + Area>(item: &T) -> String {
    format!("{}:{}", item.name(), item.area())
}

pub const MAX_SIDE: u8 = 200;
pub const UNIT: u32 = 1;
pub const TABLE: [u32; 2] = [1, 2];
// @brk const-eval: const _: u8 = MAX_SIDE + 100;
// @brk const-eval: const _: u32 = TABLE[UNIT as usize + 4];

pub fn clamp_side(side: u32) -> u32 {
    side.min(MAX_SIDE as u32)
}

#[cfg(feature = "extra")]
pub fn extra_only() -> u32 {
    7
}

pub fn scale(side: u32) -> u32 {
    // @brk cfg-gate: let _ = extra_only();
    side * UNIT
}
