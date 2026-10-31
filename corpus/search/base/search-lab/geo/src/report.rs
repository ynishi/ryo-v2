//! Text reports of shapes.

use crate::shapes::{Bounds, Shape};

pub const MAX_LINES: usize = 50;
pub const TITLE_PREFIX: &str = "report";

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub label: String,
    pub value: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub title: String,
    pub lines: Vec<Line>,
}

impl Line {
    pub fn new(label: &str, value: u32) -> Line {
        Line {
            label: label.to_string(),
            value,
        }
    }

    pub fn render(&self) -> String {
        format!("{}={}", self.label, self.value)
    }
}

impl Report {
    pub fn new(title: &str) -> Report {
        Report {
            title: title.to_string(),
            lines: Vec::new(),
        }
    }

    pub fn push(&mut self, line: Line) {
        self.lines.push(line);
    }

    pub fn count(&self) -> usize {
        self.lines.len()
    }

    pub fn total(&self) -> u32 {
        let mut total = 0;
        for line in &self.lines {
            total += line.value;
        }
        total
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.title);
        for line in &self.lines {
            out.push(' ');
            out.push_str(&line.render());
        }
        out
    }
}

pub fn line_for(shape: &Shape) -> Line {
    let name = shape.name();
    let area = shape.area();
    Line::new(name, area)
}

pub fn report_for(title: &str, shapes: &[Shape]) -> Report {
    let mut report = Report::new(title);
    for shape in shapes {
        let line = line_for(shape);
        report.push(line);
    }
    report
}

pub fn describe(shape: &Shape) -> String {
    let bounds = shape.bounds();
    describe_bounds(bounds)
}

fn describe_bounds(bounds: Bounds) -> String {
    format!("{}x{}", bounds.width, bounds.height)
}

pub fn summary(report: &Report) -> String {
    let total = report.total();
    let count = report.count();
    let text = join_summary(total, count);
    text
}

fn join_summary(total: u32, count: usize) -> String {
    let text = format!("total={} count={}", total, count);
    text
}
