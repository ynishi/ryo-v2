//! A small keyed store of shapes.

use crate::shapes::Shape;
use crate::total_area;

pub const CAPACITY: usize = 64;

pub struct Store {
    items: Vec<(String, Shape)>,
}

impl Store {
    pub fn new() -> Store {
        Store { items: Vec::new() }
    }

    pub fn put(&mut self, key: &str, shape: Shape) {
        self.items.push((key.to_string(), shape));
    }

    pub fn get(&self, key: &str) -> Option<Shape> {
        for (k, shape) in &self.items {
            if k == key {
                return Some(*shape);
            }
        }
        None
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn shapes(&self) -> Vec<Shape> {
        self.items.iter().map(|(_, shape)| *shape).collect()
    }
}

impl Default for Store {
    fn default() -> Store {
        Store::new()
    }
}

pub fn load(store: &mut Store, entries: &[(&str, Shape)]) -> usize {
    for (key, shape) in entries {
        store.put(key, *shape);
    }
    let count = store.len();
    let room = free_room(count);
    room
}

fn free_room(count: usize) -> usize {
    let room = CAPACITY.saturating_sub(count);
    room
}

pub fn fill_from(store: &mut Store, shapes: &[Shape]) {
    for shape in shapes {
        let key = shape.name();
        store.put(key, *shape);
    }
}

pub fn store_area(store: &Store) -> u32 {
    let shapes = store.shapes();
    total_area(&shapes)
}
