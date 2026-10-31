//! breakage-lab `lab-store`: a small container and ten borrow-check anchors.

use lab_shapes::{Area, Rect, Square};

pub struct Bin {
    pub items: Vec<u32>,
    pub label: String,
}

impl Bin {
    pub fn new(label: &str) -> Self {
        Bin {
            items: Vec::new(),
            label: label.to_string(),
        }
    }

    pub fn push(&mut self, v: u32) {
        self.items.push(v);
    }

    pub fn count(&self) -> u32 {
        self.items.len() as u32
    }
}

pub fn areas(sq: &Square, r: &Rect) -> u32 {
    sq.area() + r.area()
}

pub fn consume(b: Bin) -> usize {
    b.items.len()
}

pub fn after_move(b: Bin) -> usize {
    let n = consume(b);
    // @brk use-after-move: let _ = b.label.len();
    n
}

pub fn after_move_vec(v: Vec<u32>) -> usize {
    let w = v;
    // @brk use-after-move: let _ = v.len();
    w.len()
}

pub fn two_mut(v: &mut Vec<u32>) -> usize {
    let a = &mut *v;
    // @brk double-mut-borrow: let b = &mut *v; b.push(1);
    a.push(2);
    a.len()
}

pub fn two_mut_bin(b: &mut Bin) -> usize {
    let first = &mut b.items;
    // @brk double-mut-borrow: let second = &mut b.items; second.push(3);
    first.push(4);
    first.len()
}

pub fn shared_then_mut(v: &mut Vec<u32>) -> usize {
    let view = &*v;
    // @brk shared-then-mut: v.push(9);
    view.len()
}

pub fn shared_then_mut_bin(b: &mut Bin) -> usize {
    let name = &b.label;
    // @brk shared-then-mut: b.label.push('!');
    name.len()
}

pub fn move_while_borrowed(b: Bin) -> usize {
    let r = &b;
    // @brk move-while-borrowed: let _moved = consume(b);
    r.items.len()
}

pub fn move_vec_while_borrowed(v: Vec<u32>) -> usize {
    let first = v.first();
    drop(v);
    first.map_or(0, |x| *x as usize)
}

pub fn assign_while_borrowed(mut n: u32) -> u32 {
    n += 1;
    let r = &n;
    // @brk assign-while-borrowed: n = 5;
    *r
}

pub fn assign_label_while_borrowed(b: &mut Bin) -> usize {
    let r = &b.label;
    // @brk assign-while-borrowed: b.label = String::new();
    r.len()
}
