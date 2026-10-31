//! The structure the merge sees in a Markdown document: sections, the tables in them, and the
//! rows of a table. Everything works on lines without their line ends.

use std::collections::HashMap;

use crate::diff3::Item;

/// The key of the text before the first heading.
const PREAMBLE: &str = "";
/// The key of the header line and the delimiter line of a table.
const HEAD: &str = "\u{0}head";

/// Numbers a repeated key: the first keeps its text, the n-th (n >= 2) gets `\0n` appended.
#[derive(Default)]
struct Numbering(HashMap<String, usize>);

impl Numbering {
    fn key(&mut self, text: &str) -> String {
        let n = self.0.entry(text.to_owned()).or_insert(0);
        *n += 1;
        if *n == 1 {
            text.to_owned()
        } else {
            format!("{text}\u{0}{n}")
        }
    }
}

/// Tracks fenced code blocks, inside which nothing is a heading or a table.
#[derive(Default)]
struct Fence(Option<char>);

impl Fence {
    /// Whether `line` is code: inside a fence, or a fence line itself.
    fn code(&mut self, line: &str) -> bool {
        let t = line.trim_start();
        let mark = ['`', '~']
            .into_iter()
            .find(|c| t.starts_with(&String::from(*c).repeat(3)));
        match (self.0, mark) {
            (None, Some(c)) => {
                self.0 = Some(c);
                true
            }
            // A closing fence has nothing after its marks; "```rust" inside a block is code.
            (Some(open), Some(c)) if open == c && t.trim_end().chars().all(|x| x == c) => {
                self.0 = None;
                true
            }
            (open, _) => open.is_some(),
        }
    }
}

fn is_heading(line: &str) -> bool {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    (1..=6).contains(&hashes) && line[hashes..].chars().next().is_none_or(|c| c == ' ')
}

/// The sections of a document: the text before the first heading (key `""`, possibly empty),
/// then one per heading of any level, from the heading line up to the next heading. The key is
/// the heading line without trailing whitespace, numbered when repeated.
pub(crate) fn sections(lines: &[String]) -> Vec<(String, Vec<String>)> {
    let mut out = vec![(PREAMBLE.to_owned(), Vec::new())];
    let (mut fence, mut numbering) = (Fence::default(), Numbering::default());
    for line in lines {
        if !fence.code(line) && is_heading(line) {
            out.push((numbering.key(line.trim_end()), Vec::new()));
        }
        if let Some((_, body)) = out.last_mut() {
            body.push(line.clone());
        }
    }
    out
}

/// One piece of a section: a text line, or a whole table.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Block {
    /// A line outside tables.
    Line(String),
    /// A table: its key (the header line, numbered when repeated in the section) and its lines.
    Table { key: String, lines: Vec<String> },
}

impl Item for Block {
    fn key(&self) -> &str {
        match self {
            Block::Line(l) => l,
            Block::Table { key, .. } => key,
        }
    }
}

impl Block {
    /// The lines the block stands for.
    pub(crate) fn lines(&self) -> &[String] {
        match self {
            Block::Line(l) => std::slice::from_ref(l),
            Block::Table { lines, .. } => lines,
        }
    }
}

fn is_table_line(line: &str) -> bool {
    line.trim_start().starts_with('|')
}

fn is_delimiter(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// The blocks of `lines`. A table is a run of lines starting with `|` whose second line is a
/// delimiter line (`|---|---|`); a run without one is text. A table key starts with `\u{1}`, so
/// it never equals a text line.
pub(crate) fn blocks(lines: &[String]) -> Vec<Block> {
    let mut out = Vec::new();
    let (mut fence, mut numbering) = (Fence::default(), Numbering::default());
    let mut i = 0;
    while i < lines.len() {
        let code = fence.code(&lines[i]);
        let run = if code {
            0
        } else {
            lines[i..].iter().take_while(|l| is_table_line(l)).count()
        };
        if run >= 2 && is_delimiter(&lines[i + 1]) {
            out.push(Block::Table {
                key: numbering.key(&format!("\u{1}{}", lines[i].trim())),
                lines: lines[i..i + run].to_vec(),
            });
            i += run;
        } else {
            out.push(Block::Line(lines[i].clone()));
            i += 1;
        }
    }
    out
}

/// The first cell of a table row, trimmed (`\|` does not end a cell).
fn first_cell(row: &str) -> &str {
    let t = row.trim_start();
    let t = t.strip_prefix('|').unwrap_or(t);
    let mut escaped = false;
    for (i, c) in t.char_indices() {
        match c {
            '\\' => escaped = !escaped,
            '|' if !escaped => return t[..i].trim(),
            _ => escaped = false,
        }
    }
    t.trim()
}

/// The rows of a table, keyed: the header and delimiter lines as one item, then one item per
/// row keyed by its first cell (numbered when repeated).
pub(crate) fn rows(table: &[String]) -> Vec<(String, Vec<String>)> {
    let head = table.len().min(2);
    let mut out = vec![(HEAD.to_owned(), table[..head].to_vec())];
    let mut numbering = Numbering::default();
    for row in &table[head..] {
        out.push((numbering.key(first_cell(row)), vec![row.clone()]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(str::to_owned).collect()
    }

    #[test]
    fn sections_split_at_headings_outside_code() {
        let doc = lines("intro\n# A\na\n```\n# not a heading\n```\n## B\n#nospace\n## B\nb\n");
        let keys: Vec<String> = sections(&doc).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["", "# A", "## B", "## B\u{0}2"]);
        assert_eq!(sections(&doc)[1].1.len(), 5);
        // A fence line with an info string inside a block does not close it.
        let doc = lines("# A\n```\n```rust\n# not a heading\n```\n# B\n");
        let keys: Vec<String> = sections(&doc).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["", "# A", "# B"]);
    }

    #[test]
    fn tables_need_a_delimiter_line_and_rows_are_keyed_by_the_first_cell() {
        let doc = lines(
            "x\n| id | a |\n|---|:-:|\n| M-1 | p \\| q |\n|  M-2 | r |\n| M-1 | s |\ny\n| not | a table |\n",
        );
        let bs = blocks(&doc);
        assert_eq!(bs.len(), 4);
        let Block::Table { key, lines: t } = &bs[1] else {
            panic!("no table: {bs:?}")
        };
        assert_eq!(key, "\u{1}| id | a |");
        let keys: Vec<String> = rows(t).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, [HEAD, "M-1", "M-2", "M-1\u{0}2"]);
        assert_eq!(first_cell("| a \\| b | c |"), "a \\| b");
        assert_eq!(bs[3], Block::Line("| not | a table |".to_owned()));
        // A table inside a code fence is text.
        let fenced = lines("```\n| a | b |\n|---|---|\n```\n");
        assert!(blocks(&fenced).iter().all(|b| matches!(b, Block::Line(_))));
    }
}
