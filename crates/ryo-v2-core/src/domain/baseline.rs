//! The baseline difference: which Errors after an edit the load-time Errors do not account for.

use super::{Finding, FindingCode, RelPath, ScopedFindings};
use std::collections::{BTreeMap, HashSet};

type Key = (FindingCode, String, RelPath);

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
/// 1. base = restrict(baseline, &checked.files).
/// 2. Group base and checked.findings by key (code, message, path). The line is not in the key,
///    so an edit that moves a pre-existing error to another line does not make it new.
/// 3. Per key: pair each checked finding with a base finding on the same line (each base finding
///    used once); then let k = the base findings left; drop the first k remaining checked findings
///    in line order; the rest are new. So with n in base and m in checked, max(0, m - n) are new.
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
/// - C4: the same code and path with another message is new.
///   tested: domain::baseline::tests::other_message_is_new
/// - C5: analyzer codes are diffed by the same rule as rustc codes.
///   tested: domain::baseline::tests::analyzer_codes_are_diffed_too
/// - C6: the result is sorted.
///   untested: no case has several keys; owed by the backfill
pub fn new_findings(baseline: &[Finding], checked: &ScopedFindings) -> Vec<Finding> {
    let base = restrict(baseline, &checked.files);
    let mut groups: BTreeMap<Key, (Vec<u32>, Vec<u32>)> = BTreeMap::new();
    for f in &base {
        groups.entry(key(f)).or_default().0.push(f.line);
    }
    for f in &checked.findings {
        groups.entry(key(f)).or_default().1.push(f.line);
    }
    let mut out = Vec::new();
    for ((code, message, path), (mut base_lines, mut checked_lines)) in groups {
        checked_lines.sort_unstable();
        let mut unpaired = Vec::new();
        for line in checked_lines {
            if let Some(i) = base_lines.iter().position(|b| *b == line) {
                base_lines.swap_remove(i);
            } else {
                unpaired.push(line);
            }
        }
        let skip = base_lines.len().min(unpaired.len());
        for line in unpaired.into_iter().skip(skip) {
            out.push(Finding {
                code: code.clone(),
                message: message.clone(),
                path: path.clone(),
                line,
            });
        }
    }
    out.sort();
    out
}

/// Same multiset of keys (code, message, path), lines ignored. Used by the revert assertion.
///
/// # Contract
/// - C1: lines are ignored.
///   tested: domain::baseline::tests::same_findings_ignores_lines_counts_copies
/// - C2: copies are counted.
///   tested: domain::baseline::tests::same_findings_ignores_lines_counts_copies
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
    fn other_message_is_new() {
        let base = vec![f("E0599", "m", "a.rs", 1)];
        let c = checked(&["a.rs"], vec![f("E0599", "other", "a.rs", 1)]);
        assert_eq!(
            new_findings(&base, &c),
            vec![f("E0599", "other", "a.rs", 1)]
        );
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
}
