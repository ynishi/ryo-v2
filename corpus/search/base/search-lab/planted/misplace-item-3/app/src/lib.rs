//! search-lab `app`: uses `geo`.

pub mod plan;

use geo::{Circle, Rect, Report, Shape, Store};

pub const NAME: &str = "search-lab";

pub fn version() -> u32 {
    1
}

pub fn sample_shapes() -> Vec<Shape> {
    vec![
        Shape::Rect(Rect { w: 3, h: 4 }),
        Shape::Circle(Circle { r: 2 }),
        Shape::Rect(Rect { w: 5, h: 1 }),
    ]
}

pub fn sample_store() -> Store {
    let mut store = Store::new();
    let shapes = sample_shapes();
    for (index, shape) in shapes.iter().enumerate() {
        let key = format!("s{}", index);
        store.put(&key, *shape);
    }
    store
}

pub fn run() -> String {
    let store = sample_store();
    let shapes = store.shapes();
    let report = geo::report::report_for(NAME, &shapes);
    let text = finish(report);
    text
}

fn finish(report: Report) -> String {
    let text = report.render();
    text
}

pub fn total() -> u32 {
    let store = sample_store();
    let shapes = store.shapes();
    geo::total_area(&shapes) + plan::needed_width(&shapes)
}

pub struct Slot {
    pub x: u32,
    pub y: u32,
}
