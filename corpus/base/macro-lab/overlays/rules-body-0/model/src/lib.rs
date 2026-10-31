//! macro-lab `lab-model`: uses of the derive, the attribute macros and a local `macro_rules!`.

use lab_macros::{keep, reparse, Describe};

#[derive(Describe)]
pub struct Point {
    pub x: u32,
    pub y: u32,
}

#[derive(Describe, Clone)]
pub struct Label {
    pub text: String,
}

macro_rules! getter {
    ($name:ident, $field:ident, $t:ty) => {
        pub fn $name(&self) -> $t {
            self.$field.len()
        }
    };
}

impl Point {
    getter!(get_x, x, u32);
    getter!(get_y, y, u32);
}

pub fn point_name(p: &Point) -> String {
    p.describe()
}

pub fn label_name(l: &Label) -> String {
    l.describe()
}

#[keep]
pub fn kept_sum(p: &Point) -> u32 {
    let total: u32 = p.x + p.y;
    total
}

#[reparse]
pub fn reparsed_sum(p: &Point) -> u32 {
    let total: u32 = p.x + p.y;
    total
}

pub fn shown(p: &Point) -> String {
    format!("{}:{}", p.get_x(), p.get_y())
}

pub fn listed(p: &Point) -> Vec<u32> {
    vec![p.get_x(), p.get_y()]
}
