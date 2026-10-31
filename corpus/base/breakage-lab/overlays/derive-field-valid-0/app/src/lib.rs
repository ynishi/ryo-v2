//! breakage-lab `lab-app`: cross-crate callers of `lab-shapes` and `lab-store`.

use lab_shapes::{describe, label, total_area, Area, Rect, Square};
use lab_store::{after_move, Bin};

pub struct Wrapper<T> {
    pub inner: T,
}

impl<T: Area> Wrapper<T> {
    pub fn wrapped_area(&self) -> u32 {
        self.inner.area()
    }
}

pub fn squares_total(sides: &[u32]) -> u32 {
    let squares: Vec<Square> = sides.iter().map(|s| Square { side: *s }).collect();
    total_area(&squares)
}

pub fn rect_text(w: u32, h: u32) -> String {
    describe(&Rect { w, h })
}

pub fn square_label(side: u32) -> String {
    label(&Square { side })
}

pub fn wrapped(side: u32) -> u32 {
    Wrapper { inner: Square { side } }.wrapped_area()
}

pub fn clamp(side: u32) -> u32 {
    lab_shapes::clamp_side(side) + lab_shapes::scale(side)
}

pub fn fill(text: &str) -> u32 {
    let mut b = Bin::new(text);
    b.push(1);
    b.push(2);
    b.count()
}

pub fn moved_len() -> usize {
    after_move(Bin::new("m")) + lab_store::areas(&Square { side: 2 }, &Rect { w: 1, h: 3 }) as usize
}

pub fn extra() -> u32 {
    // @brk cfg-gate: let _ = lab_shapes::extra_only();
    0
}

pub struct Raw;

impl std::fmt::Debug for Raw {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("raw")
    }
}

/// Every field implements every derived trait.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plain {
    pub id: u32,
    pub name: String,
    pub tags: Vec<u8>,
    pub pair: (u8, char),
}

/// A written `Debug` impl asks nothing of the fields.
pub struct Opaque {
    pub raw: Raw,
}

impl std::fmt::Debug for Opaque {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("opaque")
    }
}

/// A generic derive: the bound is on the parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct Boxed<T> {
    pub inner: T,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Fast,
    Slow(u8),
}
