//! search-lab `geo`: shapes, reports and a store, clean and rustfmt-formatted.
//! `search-recall gen` injects the inverse of a refactor into this code.

pub mod report;
pub mod shapes;
pub mod store;

pub use report::{Line, Report};
pub use shapes::{Bounds, Circle, Rect, Shape};
pub use store::Store;

pub const LIMIT: u32 = 1000;
pub const SCALE: u32 = 3;

pub fn clamp(value: u32, limit: u32) -> u32 {
    value.min(limit)
}

pub fn scaled(value: u32) -> u32 {
    value * SCALE
}

pub fn total_area(shapes: &[Shape]) -> u32 {
    let mut total = 0;
    for shape in shapes {
        let area = shape.area();
        total += area;
    }
    total
}

pub fn total_perimeter(shapes: &[Shape]) -> u32 {
    let mut total = 0;
    for shape in shapes {
        let v2 = shape.perimeter();
        total += v2;
    }
    total
}

pub fn limited_area(shapes: &[Shape]) -> u32 {
    let value = total_area(shapes);
    clamp(value, LIMIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_add_up() {
        let shapes = [
            Shape::Rect(Rect { w: 2, h: 3 }),
            Shape::Circle(Circle { r: 1 }),
        ];
        assert_eq!(total_area(&shapes), 9);
        assert_eq!(total_perimeter(&shapes), 16);
    }
}
