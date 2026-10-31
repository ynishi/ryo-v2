//! The baseline difference: which Errors after an edit the load-time Errors do not account for.

use super::{Finding, FindingCode, RelPath, ScopedFindings};
use std::collections::{BTreeMap, HashSet};

type Key = (FindingCode, String, RelPath);

/// The base findings and the checked findings of one (code, path).
type Group<'a> = (Vec<&'a Finding>, Vec<&'a Finding>);

fn key(f: &Finding) -> Key {
    (f.code.clone(), f.message.clone(), f.path.clone())
}

/// The findings whose path is in `files` (order kept).
///
/// # Contract
/// - C1: keeps only the findings whose path is in `files`, in their original order.
///   tested: domain::baseline::tests::restrict_keeps_listed_paths_in_order
pub fn restrict(findings: &[Finding], files: &[RelPath]) -> Vec<Finding> {
    let set: HashSet<&RelPath> = files.iter().collect();
    findings
        .iter()
        .filter(|f| set.contains(&f.path))
        .cloned()
        .collect()
}

/// Errors of `checked` that the baseline does not account for. Rule:
/// 1. base = restrict(baseline, &checked.files); base and checked.findings are grouped by
///    (code, path), and each base finding is used at most once.
/// 2. Per group, in this order:
///    1. a checked finding pairs with a base finding of the same message on the same line;
///    2. per message, the k base findings of that message left over absorb the first k unpaired
///       checked findings of that message in line order (an edit that moves a pre-existing error
///       to another line does not make it new);
///    3. an unpaired checked finding pairs with a leftover base finding on the same line
///       whatever its message (an existing error whose message changed, e.g. by a type renamed
///       in it, is not new);
///    4. the rest are new.
///
///    So with n base findings and m checked ones of one message, max(0, m - n) are new.
///    Which line a new duplicate is reported on is not guaranteed when lines moved.
///
/// Result sorted (Ord).
///
/// # Contract
/// - C1: a baseline finding outside `checked.files` is ignored.
///   tested: domain::baseline::tests::baseline_outside_scope_is_ignored
/// - C2: a pre-existing error that moved to another line is not new.
///   tested: domain::baseline::tests::moved_existing_error_is_not_new
/// - C3: an added copy of an existing error is new.
///   tested: domain::baseline::tests::added_copy_of_existing_error_is_new
/// - C4: the same code and path with another message on another line is new; a finding of
///   another code or another path is never paired.
///   tested: domain::baseline::tests::other_message_on_another_line_is_new
///   tested: domain::baseline::tests::another_path_or_code_is_never_paired
/// - C5: analyzer codes are diffed by the same rule as rustc codes.
///   tested: domain::baseline::tests::analyzer_codes_are_diffed_too
/// - C6: the result is sorted.
///   tested: domain::baseline::tests::result_is_sorted
/// - C7: the same code, path and line with another message is not new.
///   tested: domain::baseline::tests::changed_message_on_the_same_line_is_not_new
///   tested: domain::baseline::tests::a_moved_error_is_paired_before_a_changed_message
/// - C8: the known cost of C7: a new error of the same code on the line of an existing error that
///   went away is taken for that error.
///   tested: domain::baseline::tests::replaced_error_on_the_same_line_is_taken_for_the_old_one
pub fn new_findings(baseline: &[Finding], checked: &ScopedFindings) -> Vec<Finding> {
    let base = restrict(baseline, &checked.files);
    let mut groups: BTreeMap<(&FindingCode, &RelPath), Group<'_>> = BTreeMap::new();
    for f in &base {
        groups.entry((&f.code, &f.path)).or_default().0.push(f);
    }
    for f in &checked.findings {
        groups.entry((&f.code, &f.path)).or_default().1.push(f);
    }
    let mut out = Vec::new();
    for (_, (base, mut found)) in groups {
        found.sort_by(|a, b| (a.line, &a.message).cmp(&(b.line, &b.message)));
        let mut used = vec![false; base.len()];
        let mut paired = vec![false; found.len()];
        // 1. same message, same line
        for (i, c) in found.iter().enumerate() {
            let hit = base
                .iter()
                .enumerate()
                .position(|(j, b)| !used[j] && b.message == c.message && b.line == c.line);
            if let Some(j) = hit {
                used[j] = true;
                paired[i] = true;
            }
        }
        // 2. same message, any line: the leftovers of a message absorb its first unpaired ones
        for (i, c) in found.iter().enumerate() {
            if paired[i] {
                continue;
            }
            let hit = base
                .iter()
                .enumerate()
                .position(|(j, b)| !used[j] && b.message == c.message);
            if let Some(j) = hit {
                used[j] = true;
                paired[i] = true;
            }
        }
        // 3. same line, any message
        for (i, c) in found.iter().enumerate() {
            if paired[i] {
                continue;
            }
            let hit = base
                .iter()
                .enumerate()
                .position(|(j, b)| !used[j] && b.line == c.line);
            if let Some(j) = hit {
                used[j] = true;
                paired[i] = true;
            }
        }
        // 4. the rest
        out.extend(
            found
                .into_iter()
                .zip(paired)
                .filter(|(_, p)| !p)
                .map(|(c, _)| c.clone()),
        );
    }
    out.sort();
    out
}

/// Same multiset of keys (code, message, path), lines ignored. Used by the revert assertion,
/// which must see a changed message, so it is exact where `new_findings` is not.
///
/// # Contract
/// - C1: lines are ignored.
///   tested: domain::baseline::tests::same_findings_ignores_lines_counts_copies
/// - C2: copies are counted.
///   tested: domain::baseline::tests::same_findings_ignores_lines_counts_copies
/// - C3: another message or another path is a difference.
///   tested: domain::baseline::tests::same_findings_sees_another_message_or_path
pub fn same_findings(a: &[Finding], b: &[Finding]) -> bool {
    let mut ka: Vec<Key> = a.iter().map(key).collect();
    let mut kb: Vec<Key> = b.iter().map(key).collect();
    ka.sort();
    kb.sort();
    ka == kb
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ErrorCode;

    fn f(code: &str, msg: &str, path: &str, line: u32) -> Finding {
        Finding {
            code: FindingCode::Rustc(ErrorCode::parse(code).unwrap()),
            message: msg.to_owned(),
            path: RelPath::parse(path).unwrap(),
            line,
        }
    }

    fn files(paths: &[&str]) -> Vec<RelPath> {
        paths.iter().map(|p| RelPath::parse(p).unwrap()).collect()
    }

    fn checked(paths: &[&str], findings: Vec<Finding>) -> ScopedFindings {
        ScopedFindings {
            files: files(paths),
            findings,
        }
    }

    #[test]
    fn restrict_keeps_listed_paths_in_order() {
        let input = vec![
            f("E0599", "m", "b.rs", 3),
            f("E0599", "m", "a.rs", 9),
            f("E0308", "n", "a.rs", 2),
        ];
        assert_eq!(
            restrict(&input, &files(&["a.rs"])),
            vec![f("E0599", "m", "a.rs", 9), f("E0308", "n", "a.rs", 2)]
        );
    }

    #[test]
    fn added_copy_of_existing_error_is_new() {
        let base = vec![f("E0599", "m", "a.rs", 10)];
        let c = checked(
            &["a.rs"],
            vec![f("E0599", "m", "a.rs", 10), f("E0599", "m", "a.rs", 14)],
        );
        assert_eq!(new_findings(&base, &c), vec![f("E0599", "m", "a.rs", 14)]);
    }

    #[test]
    fn moved_existing_error_is_not_new() {
        let base = vec![f("E0599", "m", "a.rs", 10)];
        let c = checked(&["a.rs"], vec![f("E0599", "m", "a.rs", 12)]);
        assert!(new_findings(&base, &c).is_empty());
    }

    #[test]
    fn baseline_outside_scope_is_ignored() {
        let base = vec![f("E0599", "m", "b.rs", 1)];
        let c = checked(&["a.rs"], vec![f("E0599", "m", "a.rs", 1)]);
        assert_eq!(new_findings(&base, &c), vec![f("E0599", "m", "a.rs", 1)]);
    }

    #[test]
    fn other_message_on_another_line_is_new() {
        let base = vec![f("E0599", "m", "a.rs", 1)];
        let c = checked(&["a.rs"], vec![f("E0599", "other", "a.rs", 5)]);
        assert_eq!(
            new_findings(&base, &c),
            vec![f("E0599", "other", "a.rs", 5)]
        );
    }

    #[test]
    fn changed_message_on_the_same_line_is_not_new() {
        let base = vec![f("E0599", "no method on Old", "a.rs", 4)];
        let c = checked(&["a.rs"], vec![f("E0599", "no method on New", "a.rs", 4)]);
        assert!(new_findings(&base, &c).is_empty());
    }

    #[test]
    fn replaced_error_on_the_same_line_is_taken_for_the_old_one() {
        // The price of pairing by line: the old error went away and a new one of the same code
        // appeared on its line, and the new one is not reported.
        let base = vec![f("E0599", "old", "a.rs", 3)];
        let c = checked(&["a.rs"], vec![f("E0599", "brand new", "a.rs", 3)]);
        assert!(new_findings(&base, &c).is_empty());
    }

    #[test]
    fn result_is_sorted() {
        let c = checked(
            &["a.rs", "b.rs"],
            vec![
                f("E0599", "m", "b.rs", 2),
                f("E0308", "n", "a.rs", 9),
                f("E0599", "m", "a.rs", 7),
                f("E0599", "m", "a.rs", 1),
            ],
        );
        let out = new_findings(&[], &c);
        assert_eq!(out.len(), 4);
        let mut sorted = out.clone();
        sorted.sort();
        assert_eq!(out, sorted);
        // The groups come out in (code, path) order, which is not the order of `Finding`
        // (code, message, path, line): only the final sort puts `a` before `z`.
        let c = checked(
            &["a.rs", "b.rs"],
            vec![f("E0599", "z", "a.rs", 1), f("E0599", "a", "b.rs", 1)],
        );
        assert_eq!(
            new_findings(&[], &c),
            vec![f("E0599", "a", "b.rs", 1), f("E0599", "z", "a.rs", 1)]
        );
    }

    #[test]
    fn another_path_or_code_is_never_paired() {
        let base = vec![f("E0599", "m", "a.rs", 1)];
        let c = checked(&["a.rs", "b.rs"], vec![f("E0599", "m", "b.rs", 1)]);
        assert_eq!(new_findings(&base, &c), vec![f("E0599", "m", "b.rs", 1)]);
        let c = checked(&["a.rs"], vec![f("E0308", "m", "a.rs", 1)]);
        assert_eq!(new_findings(&base, &c), vec![f("E0308", "m", "a.rs", 1)]);
    }

    #[test]
    fn a_moved_error_is_paired_before_a_changed_message() {
        // m2 moved from line 9 to 1 and m1 from 1 to 5: both are existing errors. Pairing by
        // line first would take m2@1 for m1@1 and leave m1@5 new.
        let base = vec![f("E0599", "m1", "a.rs", 1), f("E0599", "m2", "a.rs", 9)];
        let c = checked(
            &["a.rs"],
            vec![f("E0599", "m2", "a.rs", 1), f("E0599", "m1", "a.rs", 5)],
        );
        assert!(new_findings(&base, &c).is_empty());
    }

    #[test]
    fn analyzer_codes_are_diffed_too() {
        let mk = |line| Finding {
            code: FindingCode::Analyzer("unresolved-macro-call".to_owned()),
            message: "m".to_owned(),
            path: RelPath::parse("a.rs").unwrap(),
            line,
        };
        let base = vec![mk(1)];
        let c = checked(&["a.rs"], vec![mk(1), mk(5)]);
        assert_eq!(new_findings(&base, &c), vec![mk(5)]);
        let c = checked(&["a.rs"], vec![mk(3)]);
        assert!(new_findings(&base, &c).is_empty());
    }

    #[test]
    fn same_findings_ignores_lines_counts_copies() {
        let a = vec![f("E0599", "m", "a.rs", 1)];
        let b = vec![f("E0599", "m", "a.rs", 9)];
        assert!(same_findings(&a, &b));
        let two = vec![f("E0599", "m", "a.rs", 1), f("E0599", "m", "a.rs", 2)];
        assert!(!same_findings(&a, &two));
        assert!(!same_findings(&two, &a));
    }

    #[test]
    fn same_findings_sees_another_message_or_path() {
        let a = vec![f("E0599", "m", "a.rs", 1)];
        assert!(!same_findings(&a, &[f("E0599", "other", "a.rs", 1)]));
        assert!(!same_findings(&a, &[f("E0599", "m", "b.rs", 1)]));
    }
}
