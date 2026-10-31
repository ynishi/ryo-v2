//! Shapes and their measures.

pub const UNIT: u32 = 1;
pub const MAX_SIDE: u32 = 500;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Circle {
    pub r: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape {
    Rect(Rect),
    Circle(Circle),
}

impl Rect {
    pub fn area(&self) -> u32 {
        self.w * self.h
    }

    pub fn perimeter(&self) -> u32 {
        2 * (self.w + self.h)
    }

    pub fn bounds(&self) -> Bounds {
        Bounds {
            width: self.w,
            height: self.h,
        }
    }
}

impl Circle {
    pub fn area(&self) -> u32 {
        3 * self.r * self.r
    }

    pub fn perimeter(&self) -> u32 {
        6 * self.r
    }

    pub fn bounds(&self) -> Bounds {
        Bounds {
            width: 2 * self.r,
            height: 2 * self.r,
        }
    }
}

impl Shape {
    pub fn area(&self) -> u32 {
        match self {
            Shape::Rect(rect) => rect.area(),
            Shape::Circle(circle) => circle.area(),
        }
    }

    pub fn perimeter(&self) -> u32 {
        match self {
            Shape::Rect(rect) => rect.perimeter(),
            Shape::Circle(circle) => circle.perimeter(),
        }
    }

    pub fn bounds(&self) -> Bounds {
        match self {
            Shape::Rect(rect) => rect.bounds(),
            Shape::Circle(circle) => circle.bounds(),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Shape::Rect(_) => "rect",
            Shape::Circle(_) => "circle",
        }
    }
}

pub fn fits(shape: &Shape, outer: Bounds) -> bool {
    let wide = wider(outer, shape.bounds());
    !wide
}

fn wider(outer: Bounds, inner: Bounds) -> bool {
    let wide = inner.width > outer.width || inner.height > outer.height;
    wide
}

pub fn weight(shapes: &[Shape]) -> u32 {
    let mut total = 0;
    for shape in shapes {
        let bounds = shape.bounds();
        total += weigh(bounds);
    }
    total
}

fn weigh(bounds: Bounds) -> u32 {
    bounds.width * bounds.height
}

pub fn square(side: u32) -> Rect {
    Rect {
        w: side.min(MAX_SIDE),
        h: side.min(MAX_SIDE),
    }
}

pub fn double(value: u32) -> u32 {
    value * 2
}
