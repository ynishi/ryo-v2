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

#[derive(Clone)]
pub struct Holder {
    pub id: u32,
    pub raw: Raw,
}
