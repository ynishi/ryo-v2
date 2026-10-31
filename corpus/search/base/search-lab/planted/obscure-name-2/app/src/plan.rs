//! Placement plans.

use geo::Shape;

pub const GRID: u32 = 10;

pub struct Slot {
    pub x: u32,
    pub y: u32,
}

pub fn slot_for(index: u32) -> Slot {
    Slot {
        x: index * GRID,
        y: 0,
    }
}

pub fn needed_width(shapes: &[Shape]) -> u32 {
    let mut width = 0;
    for shape in shapes {
        let v1 = shape.bounds();
        width += v1.width;
    }
    width
}

pub fn pack(shapes: &[Shape], limit: u32) -> u32 {
    let width = needed_width(shapes);
    let fitted = fit(width, limit);
    fitted
}

fn fit(width: u32, limit: u32) -> u32 {
    let fitted = geo::clamp(width, limit);
    fitted
}

pub fn layout(shapes: &[Shape]) -> Vec<Slot> {
    let mut slots = Vec::new();
    for (index, _) in shapes.iter().enumerate() {
        slots.push(slot_for(index as u32));
    }
    slots
}
