//! Merge driver of the ryo-v2 ledgers (`CHECK_LEDGER.md`, `SEARCH_LEDGER.md`).
//!
//! The ledgers grow by table rows and by sections, and parallel branches add, remove and edit
//! them in the same places. A line-level merge reports a conflict whenever two such edits touch;
//! [`merge`] merges by structure instead:
//!
//! 1. the document is split into sections at its headings (any level), keyed by the heading
//!    line; sections are merged as a keyed list;
//! 2. a section both sides changed is merged line by line, with each table taken as one item
//!    (keyed by its header line);
//! 3. a table both sides changed is merged as a keyed list of rows, keyed by the first cell.
//!
//! A keyed list merges as `keyed::merge` says: one-sided changes are taken, two different
//! changes of one key conflict, the order is that of ours with the new keys of theirs after
//! their predecessor in theirs. A conflict is written with ordinary conflict markers around
//! the smallest unit that conflicts: a row, a stretch of lines, or a section that one side
//! removed and the other changed.
//!
//! The binary `ledger-merge` is the git merge driver; `.gitattributes` names the files it is
//! used for and `just ledger-merge-install` registers it in a checkout.

mod diff3;
mod keyed;
mod markdown;

use diff3::Chunk;
use keyed::Outcome;
use markdown::Block;

/// The marker size git uses when it passes none.
pub const DEFAULT_MARKER_SIZE: usize = 7;

/// The result of a merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    /// The merged text, conflict markers included.
    pub text: String,
    /// How many conflicts the text holds (each is one marked stretch).
    pub conflicts: usize,
}

/// The output being written.
struct Out {
    lines: Vec<String>,
    conflicts: usize,
    marker_size: usize,
}

impl Out {
    fn push(&mut self, lines: &[String]) {
        self.lines.extend(lines.iter().cloned());
    }

    fn conflict(&mut self, ours: &[String], theirs: &[String]) {
        let n = self.marker_size;
        self.lines.push(format!("{} ours", "<".repeat(n)));
        self.push(ours);
        self.lines.push("=".repeat(n));
        self.push(theirs);
        self.lines.push(format!("{} theirs", ">".repeat(n)));
        self.conflicts += 1;
    }
}

/// The lines of `text` without their `\n` (a `\r` stays part of the line).
fn split(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// Merges `ours` and `theirs`, two descendants of `base`, by section and table row.
///
/// `marker_size` is the length of the conflict markers (git's `%L`).
///
/// # Contract
/// - C1: a table row added, removed or edited on one side only is taken from that side, without
///   a conflict, whatever the other side did to the rows next to it.
///   tested: tests::rows_added_and_removed_on_both_sides_merge
///   tested: tests::cells_edited_on_one_side_beside_edits_of_the_other_merge
/// - C2: a row both sides changed differently (two edits, or an edit and a removal) is the only
///   thing between conflict markers, and each such row counts one conflict; the rest of the
///   document is merged.
///   tested: tests::a_row_changed_differently_on_both_sides_conflicts_alone
///   tested: tests::a_row_removed_on_one_side_and_edited_on_the_other_conflicts
/// - C3: a section added on one side is kept, also when the other side adds another section or
///   changes the text of the section before it; the order is that of ours, with a new section of
///   theirs after its predecessor in theirs and after the new sections of ours there.
///   tested: tests::sections_added_on_both_sides_are_both_kept
/// - C4: lines outside tables that both sides changed differently are merged line by line, with
///   conflict markers around the lines that conflict.
///   tested: tests::text_changed_on_both_sides_falls_back_to_lines
/// - C5: when one side equals `base`, or both sides are equal, the result is the other side's text
///   (byte for byte) without a conflict.
///   tested: tests::one_sided_merges_return_that_side
/// - C6: a table whose header line one side changed is a different table: if the other side
///   changed the table as well, the whole table is one conflict.
///   tested: tests::a_changed_header_makes_a_table_conflict_as_a_whole
/// - C8: a section both sides added under the same heading, and any two insertions at one place
///   in a section, are merged against the tables both have (each with its header and the rows
///   equal on both sides): the rows each side adds are kept. Text that differs conflicts as a
///   whole, also when the two texts share a line, and a stretch both sides changed that had text
///   in `base` is never merged this way. A block both insert that the two place differently
///   makes the two insertions one conflict, and a table both add whose header lines differ
///   conflicts on its header.
///   tested: tests::a_section_added_on_both_sides_with_different_bodies_merges_its_lines
///   tested: tests::insertions_of_the_same_lines_in_another_order_conflict_instead_of_doubling
///   tested: tests::a_table_both_add_with_different_delimiter_lines_conflicts_on_the_header
///   tested: tests::different_insertions_sharing_a_blank_line_conflict
/// - C7 [free]: line ends: a `\r` stays part of its line, and a merged text (one that is not
///   one side unchanged) ends with a line end.
pub fn merge(base: &str, ours: &str, theirs: &str, marker_size: usize) -> Merged {
    let clean = |text: &str| Merged {
        text: text.to_owned(),
        conflicts: 0,
    };
    if ours == theirs || theirs == base {
        return clean(ours);
    }
    if ours == base {
        return clean(theirs);
    }
    let mut out = Out {
        lines: Vec::new(),
        conflicts: 0,
        marker_size,
    };
    let (b, o, t) = (split(base), split(ours), split(theirs));
    let (sb, so, st) = (
        markdown::sections(&b),
        markdown::sections(&o),
        markdown::sections(&t),
    );
    for outcome in keyed::merge(&sb, &so, &st) {
        match outcome {
            Outcome::Take(lines) => out.push(lines),
            Outcome::Conflict {
                base,
                ours: Some(o),
                theirs: Some(t),
            } => merge_section(base.map_or(&[][..], Vec::as_slice), o, t, &mut out),
            Outcome::Conflict { ours, theirs, .. } => out.conflict(
                ours.map_or(&[][..], Vec::as_slice),
                theirs.map_or(&[][..], Vec::as_slice),
            ),
        }
    }
    let mut text = out.lines.join("\n");
    text.push('\n');
    Merged {
        text,
        conflicts: out.conflicts,
    }
}

/// A section both sides changed: line by line, tables as items.
fn merge_section(base: &[String], ours: &[String], theirs: &[String], out: &mut Out) {
    merge_blocks(
        &markdown::blocks(base),
        &markdown::blocks(ours),
        &markdown::blocks(theirs),
        out,
    );
}

fn flat(blocks: &[Block]) -> Vec<String> {
    blocks
        .iter()
        .flat_map(|b| b.lines().iter().cloned())
        .collect()
}

fn merge_blocks(base: &[Block], ours: &[Block], theirs: &[Block], out: &mut Out) {
    for chunk in diff3::merge(base, ours, theirs) {
        match chunk {
            Chunk::Stable {
                base: Block::Table { lines: b, .. },
                ours: Block::Table { lines: o, .. },
                theirs: Block::Table { lines: t, .. },
            } => merge_table(b, o, t, out),
            // A line: its key is its text, so all three are equal.
            Chunk::Stable { ours, .. } => out.push(ours.lines()),
            Chunk::Take(blocks) => out.push(&flat(blocks)),
            // Two insertions at one place: merged against what they share, so that a table both
            // add keeps the rows of each. The common part is not empty, so the stretches left
            // are smaller and the recursion ends.
            // A block both sides insert that the alignment does not pair (the two put it in a
            // different order around the shared ones) would be taken from each side and written
            // twice, so then the two insertions are one conflict.
            Chunk::Conflict { base, ours, theirs } if base.is_empty() => {
                let common = shared(ours, theirs);
                if common.is_empty() || unpaired_in_both(ours, theirs) {
                    out.conflict(&flat(ours), &flat(theirs));
                } else {
                    merge_blocks(&common, ours, theirs, out);
                }
            }
            Chunk::Conflict { ours, theirs, .. } => out.conflict(&flat(ours), &flat(theirs)),
        }
    }
}

/// Whether a key that the alignment of `ours` with `theirs` leaves unpaired is in both.
fn unpaired_in_both(ours: &[Block], theirs: &[Block]) -> bool {
    use diff3::Item as _;
    let aligned = diff3::align(ours, theirs);
    let mut paired = vec![false; theirs.len()];
    for j in aligned.iter().flatten() {
        paired[*j] = true;
    }
    let theirs_unpaired: std::collections::HashSet<&str> = theirs
        .iter()
        .zip(&paired)
        .filter(|(_, p)| !**p)
        .map(|(t, _)| t.key())
        .collect();
    ours.iter()
        .zip(&aligned)
        .any(|(o, j)| j.is_none() && theirs_unpaired.contains(o.key()))
}

/// What two insertions at one place share, as the base to merge them against: the tables both
/// have (aligned by key, in order), each with its header and the rows equal on both sides. Text
/// lines are not shared: two different texts anchored on a common line (a blank line) would be
/// interleaved without a conflict, so text that differs conflicts as a whole. When the two headers
/// differ (the key is the trimmed header line, so whitespace or the delimiter line can differ) the
/// shared table is empty, so the header conflicts instead of one side's being taken.
fn shared(ours: &[Block], theirs: &[Block]) -> Vec<Block> {
    diff3::align(ours, theirs)
        .into_iter()
        .zip(ours)
        .filter_map(|(j, o)| Some((o, &theirs[j?])))
        .filter_map(|(o, t)| match (o, t) {
            (Block::Table { key, lines: lo }, Block::Table { lines: lt, .. }) => {
                let theirs_rows = markdown::rows(lt);
                let ours_rows = markdown::rows(lo);
                let mut lines = Vec::new();
                if ours_rows.first() == theirs_rows.first() {
                    for (k, row) in ours_rows {
                        if theirs_rows.iter().any(|(tk, tr)| *tk == k && *tr == row) {
                            lines.extend(row);
                        }
                    }
                }
                Some(Block::Table {
                    key: key.clone(),
                    lines,
                })
            }
            _ => None,
        })
        .collect()
}

/// A table both sides changed: row by row.
fn merge_table(base: &[String], ours: &[String], theirs: &[String], out: &mut Out) {
    let (rb, ro, rt) = (
        markdown::rows(base),
        markdown::rows(ours),
        markdown::rows(theirs),
    );
    for outcome in keyed::merge(&rb, &ro, &rt) {
        match outcome {
            Outcome::Take(lines) => out.push(lines),
            Outcome::Conflict { ours, theirs, .. } => out.conflict(
                ours.map_or(&[][..], Vec::as_slice),
                theirs.map_or(&[][..], Vec::as_slice),
            ),
        }
    }
}

/// The line numbers (from 1) of the conflict markers in `text`: a line that starts with seven or
/// more `<`, `>` or `|` followed by a space or the line end, or a line of seven or more `=` only.
///
/// # Contract
/// - C1: every marker line `merge` writes is found, at any marker size from 7, and a table row,
///   a heading or a rule of fewer than seven characters is not.
///   tested: tests::conflict_markers_are_found_and_table_rows_are_not
pub fn conflict_markers(text: &str) -> Vec<usize> {
    let is_marker = |line: &str| {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let Some(c) = line.chars().next() else {
            return false;
        };
        let rest = line.trim_start_matches(c);
        line.len() - rest.len() >= DEFAULT_MARKER_SIZE
            && match c {
                '<' | '>' | '|' => rest.is_empty() || rest.starts_with(' '),
                '=' => rest.is_empty(),
                _ => false,
            }
    };
    text.split('\n')
        .enumerate()
        .filter(|(_, l)| is_marker(l))
        .map(|(i, _)| i + 1)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ledger in the shape of `CHECK_LEDGER.md`: a title, Open work, Counts with a subsection,
    /// Gaps.
    const BASE: &str = "\
# Check ledger

Intro text.

## Open work

Work that is known and not done.

| id | area | what is wrong | direction | state |
|---|---|---|---|---|
| M-1 | rules | text match only | engine specs | open |
| M-6 | verifier gaps | no outer check | a runner | open |
| M-7 | risk marks | build.rs files | mark them | open |
| R-9 | oracle floors | hit only | floor exact | open |

## Counts

Corpus: 5 workspaces.

### after: R-7

| group | hit |
|---|---|
| copy-impl | 6 |

## Gaps

| id | group | count |
|---|---|---|
| G-B-1 | borrow | 10 cases |
";

    fn edit(text: &str, pairs: &[(&str, &str)]) -> String {
        let mut s = text.to_owned();
        for (from, to) in pairs {
            assert!(s.contains(from), "fixture has no {from:?}");
            s = s.replacen(from, to, 1);
        }
        s
    }

    fn run(base: &str, ours: &str, theirs: &str) -> Merged {
        merge(base, ours, theirs, DEFAULT_MARKER_SIZE)
    }

    /// How many conflicts a plain line-level merge of the whole files has: each shape below is
    /// one a line merge does not resolve.
    fn line_conflicts(base: &str, ours: &str, theirs: &str) -> usize {
        let (b, o, t) = (split(base), split(ours), split(theirs));
        diff3::merge(&b, &o, &t)
            .iter()
            .filter(|c| matches!(c, Chunk::Conflict { .. }))
            .count()
    }

    const M1: &str = "| M-1 | rules | text match only | engine specs | open |\n";
    const M6: &str = "| M-6 | verifier gaps | no outer check | a runner | open |\n";
    const M7: &str = "| M-7 | risk marks | build.rs files | mark them | open |\n";
    const R9: &str = "| R-9 | oracle floors | hit only | floor exact | open |\n";

    #[test]
    fn rows_added_and_removed_on_both_sides_merge() {
        // ours: M-6 done (removed), M-15 added at the end; theirs: R-9 done, two new rows.
        let m15 = "| M-15 | resolution output | blockings | list them | open |\n";
        let la = "| L13-a | errors | reach | map it | open |\n";
        let lb = "| L13-b | attribution | include! | the call | open |\n";
        let ours = edit(BASE, &[(M6, ""), (R9, &format!("{R9}{m15}"))]);
        let theirs = edit(BASE, &[(R9, &format!("{la}{lb}"))]);
        assert!(line_conflicts(BASE, &ours, &theirs) > 0);
        let m = run(BASE, &ours, &theirs);
        assert_eq!(m.conflicts, 0, "{}", m.text);
        let want = edit(BASE, &[(M6, ""), (R9, &format!("{m15}{la}{lb}"))]);
        assert_eq!(m.text, want);
    }

    #[test]
    fn cells_edited_on_one_side_beside_edits_of_the_other_merge() {
        let m6 = M6.replace("| open |", "| open: valve in place |");
        let m7 = M7.replace("mark them", "mark them; read Cargo.toml");
        let m1 = M1.replace("rules", "rules / intents");
        let ours = edit(BASE, &[(M6, &m6)]);
        let theirs = edit(BASE, &[(M7, &m7), (M1, &m1)]);
        assert!(line_conflicts(BASE, &ours, &theirs) > 0);
        let m = run(BASE, &ours, &theirs);
        assert_eq!(m.conflicts, 0, "{}", m.text);
        assert_eq!(m.text, edit(BASE, &[(M1, &m1), (M6, &m6), (M7, &m7)]));
    }

    #[test]
    fn sections_added_on_both_sides_are_both_kept() {
        // The shape of a real merge: ours put a new "### after:" section at the top of Counts,
        // theirs appended a paragraph to the Counts text and a section of its own there.
        let r7 = "### after: R-7\n";
        let ours = edit(
            BASE,
            &[(
                r7,
                &format!(
                    "### after: R-8\n\n| group | hit |\n|---|---|\n| derive-field | 13 |\n\n{r7}"
                ),
            )],
        );
        let theirs = edit(
            BASE,
            &[
                (
                    "Corpus: 5 workspaces.\n",
                    "Corpus: 5 workspaces.\n\nR-4 matching: unchanged.\n",
                ),
                (r7, &format!("### after: R-4\n\nNo floor moves.\n\n{r7}")),
            ],
        );
        assert!(line_conflicts(BASE, &ours, &theirs) > 0);
        let m = run(BASE, &ours, &theirs);
        assert_eq!(m.conflicts, 0, "{}", m.text);
        let want = edit(
            BASE,
            &[
                (
                    "Corpus: 5 workspaces.\n",
                    "Corpus: 5 workspaces.\n\nR-4 matching: unchanged.\n",
                ),
                (
                    r7,
                    &format!(
                        "### after: R-8\n\n| group | hit |\n|---|---|\n| derive-field | 13 |\n\n### after: R-4\n\nNo floor moves.\n\n{r7}"
                    ),
                ),
            ],
        );
        assert_eq!(m.text, want);
    }

    #[test]
    fn a_row_changed_differently_on_both_sides_conflicts_alone() {
        let m7o = M7.replace("| open |", "| done |");
        let m7t = M7.replace("| open |", "| deferred |");
        let g = ("| 10 cases |", "| 11 cases |");
        let m16 = "| M-16 | provenance | fallbacks | a test | open |\n";
        let ours = edit(BASE, &[(M7, &m7o), (R9, &format!("{R9}{m16}"))]);
        let theirs = edit(BASE, &[(M7, &m7t), g]);
        let m = run(BASE, &ours, &theirs);
        assert_eq!(m.conflicts, 1, "{}", m.text);
        let marked = format!("<<<<<<< ours\n{m7o}=======\n{m7t}>>>>>>> theirs\n");
        assert_eq!(
            m.text,
            edit(BASE, &[(M7, &marked), (R9, &format!("{R9}{m16}")), g])
        );
        assert_eq!(conflict_markers(&m.text).len(), 3);
    }

    #[test]
    fn a_row_removed_on_one_side_and_edited_on_the_other_conflicts() {
        let r9t = R9.replace("hit only", "hit only (copy-impl)");
        let ours = edit(BASE, &[(R9, "")]);
        let theirs = edit(BASE, &[(R9, &r9t), (M1, &M1.replace("open", "closed"))]);
        let m = run(BASE, &ours, &theirs);
        assert_eq!(m.conflicts, 1, "{}", m.text);
        let marked = format!("<<<<<<< ours\n=======\n{r9t}>>>>>>> theirs\n");
        assert_eq!(
            m.text,
            edit(BASE, &[(R9, &marked), (M1, &M1.replace("open", "closed"))])
        );
    }

    #[test]
    fn text_changed_on_both_sides_falls_back_to_lines() {
        let ours = edit(BASE, &[("Intro text.", "Intro A."), (M6, "")]);
        let theirs = edit(
            BASE,
            &[("Intro text.", "Intro B."), ("10 cases", "9 cases")],
        );
        let m = run(BASE, &ours, &theirs);
        assert_eq!(m.conflicts, 1, "{}", m.text);
        assert_eq!(
            m.text,
            edit(
                BASE,
                &[
                    (
                        "Intro text.\n",
                        "<<<<<<< ours\nIntro A.\n=======\nIntro B.\n>>>>>>> theirs\n"
                    ),
                    (M6, ""),
                    ("10 cases", "9 cases"),
                ]
            )
        );
        // A section removed by one side and changed by the other is one conflict, the section
        // against nothing.
        let ours = edit(
            BASE,
            &[(
                "## Gaps\n\n| id | group | count |\n|---|---|---|\n| G-B-1 | borrow | 10 cases |\n",
                "",
            )],
        );
        let theirs = edit(BASE, &[("10 cases", "9 cases")]);
        let m = run(BASE, &ours, &theirs);
        assert_eq!(m.conflicts, 1, "{}", m.text);
        assert!(m.text.ends_with("<<<<<<< ours\n=======\n## Gaps\n\n| id | group | count |\n|---|---|---|\n| G-B-1 | borrow | 9 cases |\n>>>>>>> theirs\n"), "{}", m.text);
    }

    #[test]
    fn a_section_added_on_both_sides_with_different_bodies_merges_its_lines() {
        // Both add "## Notes" at the end: the same heading, a shared table with one row each.
        let notes = |row: &str, text: &str| {
            format!("{BASE}\n## Notes\n\n{text}\n\n| id | note |\n|---|---|\n{row}")
        };
        let ours = notes("| N-1 | a |\n", "Shared text.");
        let theirs = notes("| N-2 | b |\n", "Shared text.");
        let m = run(BASE, &ours, &theirs);
        assert_eq!(m.conflicts, 0, "{}", m.text);
        assert_eq!(m.text, notes("| N-1 | a |\n| N-2 | b |\n", "Shared text."));
        // Different text in the two new sections conflicts as a whole; the table still merges.
        let theirs = notes("| N-2 | b |\n", "Other text.");
        let m = run(BASE, &ours, &theirs);
        assert_eq!(m.conflicts, 1, "{}", m.text);
        assert_eq!(
            m.text,
            format!(
                "{BASE}\n<<<<<<< ours\n## Notes\n\nShared text.\n\n=======\n## Notes\n\nOther text.\n\n>>>>>>> theirs\n| id | note |\n|---|---|\n| N-1 | a |\n| N-2 | b |\n"
            )
        );
    }

    #[test]
    fn different_insertions_sharing_a_blank_line_conflict() {
        // Both add "## Notes": ours a blank line then foo, theirs bar then a blank line. Merged
        // around the shared blank line they would interleave as bar, blank, foo.
        let ours = format!("{BASE}\n## Notes\n\nfoo\n");
        let theirs = format!("{BASE}\n## Notes\nbar\n\n");
        let m = run(BASE, &ours, &theirs);
        assert_eq!(m.conflicts, 1, "{}", m.text);
        // A stretch that had text in base is not merged around a table both put there: ours
        // replaced "Intro text." by a line and a table, theirs by the table and another line.
        let t = "| k | v |\n|---|---|\n| K-1 | x |\n";
        let ours = BASE.replacen("Intro text.\n", &format!("Intro B.\n{t}"), 1);
        let theirs = BASE.replacen("Intro text.\n", &format!("{t}Intro D.\n"), 1);
        let m = run(BASE, &ours, &theirs);
        assert_eq!(m.conflicts, 1, "{}", m.text);
    }

    #[test]
    fn insertions_of_the_same_lines_in_another_order_conflict_instead_of_doubling() {
        // Both add "## Notes" with the lines x and y, in opposite orders: x is not written twice.
        let ours = format!("{BASE}\n## Notes\n\nx\ny\n");
        let theirs = format!("{BASE}\n## Notes\n\ny\nx\n");
        let m = run(BASE, &ours, &theirs);
        assert_eq!(m.conflicts, 1, "{}", m.text);
        assert!(
            m.text.ends_with(
                "<<<<<<< ours\n## Notes\n\nx\ny\n=======\n## Notes\n\ny\nx\n>>>>>>> theirs\n"
            ),
            "{}",
            m.text
        );
    }

    #[test]
    fn a_table_both_add_with_different_delimiter_lines_conflicts_on_the_header() {
        let notes =
            |delim: &str, row: &str| format!("{BASE}\n## Notes\n\n| id | note |\n{delim}\n{row}");
        let ours = notes("|---|---|", "| N-1 | a |\n");
        let theirs = notes("|:--|---|", "| N-2 | b |\n");
        let m = run(BASE, &ours, &theirs);
        assert_eq!(m.conflicts, 1, "{}", m.text);
        assert!(
            m.text.contains(
                "<<<<<<< ours\n| id | note |\n|---|---|\n=======\n| id | note |\n|:--|---|\n>>>>>>> theirs\n| N-1 | a |\n| N-2 | b |\n"
            ),
            "{}",
            m.text
        );
    }

    #[test]
    fn one_sided_merges_return_that_side() {
        let other = edit(BASE, &[(M6, "")]);
        let no_end = BASE.trim_end().to_owned();
        for (b, o, t, want) in [
            (BASE, BASE, other.as_str(), other.as_str()),
            (BASE, other.as_str(), BASE, other.as_str()),
            (BASE, other.as_str(), other.as_str(), other.as_str()),
            (BASE, no_end.as_str(), BASE, no_end.as_str()),
            (BASE, BASE, no_end.as_str(), no_end.as_str()),
            (BASE, no_end.as_str(), no_end.as_str(), no_end.as_str()),
        ] {
            assert_eq!(
                run(b, o, t),
                Merged {
                    text: want.to_owned(),
                    conflicts: 0
                }
            );
        }
    }

    #[test]
    fn a_changed_header_makes_a_table_conflict_as_a_whole() {
        let ours = edit(
            BASE,
            &[(
                "| id | group | count |\n|---|---|---|\n| G-B-1 | borrow | 10 cases |",
                "| id | group | count | state |\n|---|---|---|---|\n| G-B-1 | borrow | 10 cases | open |",
            )],
        );
        let theirs = edit(BASE, &[("10 cases", "9 cases"), (M6, "")]);
        let m = run(BASE, &ours, &theirs);
        assert_eq!(m.conflicts, 1, "{}", m.text);
        assert!(
            m.text
                .contains("<<<<<<< ours\n| id | group | count | state |\n"),
            "{}",
            m.text
        );
        assert!(!m.text.contains(M6), "{}", m.text);
    }

    #[test]
    fn conflict_markers_are_found_and_table_rows_are_not() {
        let m = run(
            BASE,
            &edit(BASE, &[("Intro text.", "Intro A.")]),
            &edit(BASE, &[("Intro text.", "Intro B.")]),
        );
        assert_eq!(conflict_markers(&m.text), [3, 5, 7]);
        let wide = merge(
            BASE,
            &edit(BASE, &[("Intro text.", "Intro A.")]),
            &edit(BASE, &[("Intro text.", "Intro B.")]),
            9,
        );
        assert_eq!(conflict_markers(&wide.text), [3, 5, 7]);
        assert_eq!(conflict_markers(BASE), Vec::<usize>::new());
        assert_eq!(
            conflict_markers("| a |\n||| b\n======\n<<<<<<<x\n## h\n=======\r\n>>>>>>>\n"),
            [6, 7]
        );
        // The base marker of a diff3-style conflict, and markers with a `\r`.
        assert_eq!(
            conflict_markers("||||||| base\n|||||||\n<<<<<<< ours\r\n| x | y |\n"),
            [1, 2, 3]
        );
    }
}
