//! The outcome of verifying edits.

use super::baseline::new_findings;
use super::{Diagnostics, Finding, RelPath, ScopedFindings, UnseenClass};
use std::collections::BTreeSet;

/// The errors the edits introduced.
///
/// There is no `passed` field: pass/fail is derived from `new_errors`, so a method
/// ([`Verdict::passed`]) cannot disagree with it, while `new_errors` stays a plain pub field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// Errors the baseline does not account for.
    pub new_errors: Diagnostics,
    /// The classes of error the check did not see, as observed in it.
    pub unseen: BTreeSet<UnseenClass>,
}

impl Verdict {
    /// Builds a verdict from already-computed new errors (also used for "no edits"); nothing was
    /// observed.
    pub fn from_diagnostics(new_errors: Diagnostics) -> Self {
        Self {
            new_errors,
            unseen: BTreeSet::new(),
        }
    }

    /// `Diagnostics::from_findings(&new_findings(baseline, checked))`.
    ///
    /// # Contract
    /// - C1: rustc codes land in `new_errors.errors`, analyzer codes in `new_errors.unmapped`.
    ///   tested: domain::verdict::tests::from_baseline_splits_codes
    /// - C2: `unseen` is `checked.unseen()`.
    ///   tested: domain::verdict::tests::from_baseline_carries_the_observed_unseen
    pub fn from_baseline(baseline: &[Finding], checked: &ScopedFindings) -> Self {
        Self {
            unseen: checked.unseen(),
            ..Self::from_diagnostics(Diagnostics::from_findings(&new_findings(baseline, checked)))
        }
    }

    /// The verdict of the state of a level, and that state's findings: the diagnosis of the level
    /// put on top of the findings of the state below, kept to `files`, against the baseline. A
    /// flat check is the one-level case (`Verdict::of_check`): nothing below, nothing deleted, and
    /// its own files. `app::verdict_of` forms the verdict of a check of a decision here, and
    /// `app::verify` that of a verify; a stacked fork forms that of a level here.
    ///
    /// The composition takes each file from its latest diagnosis, and a finding goes with the
    /// file it is on. A finding on no file of its diagnosis would stay from below after the file
    /// that produced it was diagnosed again, so a stacked verdict and the flat check would part;
    /// the port rules such a finding out (`Stacked::diagnose` C9, `Verifier::check` C1).
    ///
    /// # Contract
    /// - C1: the findings are `level.on_top_of(below, deleted).within(files)`, and the verdict is
    ///   `Verdict::from_baseline(baseline, ..)` of them.
    ///   tested: domain::verdict::tests::of_level_composes_then_keeps_the_files_of_the_check
    /// - C2: with `ScopedFindings::default()` below, nothing deleted and `files` the files of
    ///   `level`, the findings are `level` and the verdict is `Verdict::from_baseline(baseline,
    ///   level)`, for a `level` whose files are sorted and unique, whose findings are sorted and
    ///   whose classes name only its files (as a diagnosis names them, `Stacked::diagnose` C8).
    ///   tested: domain::verdict::tests::of_level_with_nothing_below_is_the_check
    /// - C3: for the levels of the fork of any `Stacked`, each diagnosed at one scope and composed
    ///   from the bottom with the paths each deletes, and `files` the `Stacked::files_of` of the
    ///   top level at that scope, the findings and the verdict are those of the one-level case of
    ///   `Verifier::check`, at that scope, of the edits that take the base to the top level's
    ///   state: the same files, findings and `unseen_at`. It rests on the port's clauses
    ///   `Stacked::diagnose` C1, C3, C8 and C9, `Stacked::write` C3 and `Stacked::files_of` C1,
    ///   and, for a search's composed edits to be those edits, on `stage` C3 (a step only writes).
    ///   tested: ryo-v2-ra tests/stack.rs files_of_and_of_level_equal_the_check_over_a_stack
    ///   tested: ryo-v2-ra tests/stack.rs a_finding_is_on_a_file_of_its_diagnosis
    pub fn of_level(
        baseline: &[Finding],
        below: &ScopedFindings,
        level: &ScopedFindings,
        deleted: &[RelPath],
        files: &[RelPath],
    ) -> (Verdict, ScopedFindings) {
        let found = level.on_top_of(below, deleted).within(files);
        (Self::from_baseline(baseline, &found), found)
    }

    /// The verdict of a flat check: `Verdict::of_level` of `checked` with nothing below, nothing
    /// deleted and the check's own files.
    ///
    /// # Contract
    /// - C1: the verdict is `Verdict::of_level(baseline, &ScopedFindings::default(), checked,
    ///   &[], &checked.files)`.
    ///   tested: domain::verdict::tests::of_level_with_nothing_below_is_the_check
    pub fn of_check(baseline: &[Finding], checked: &ScopedFindings) -> Self {
        Self::of_level(
            baseline,
            &ScopedFindings::default(),
            checked,
            &[],
            &checked.files,
        )
        .0
    }

    /// `new_errors.is_empty()`.
    ///
    /// # Contract
    /// - C1: passed if and only if `new_errors` is empty.
    ///   tested: domain::verdict::tests::passed_iff_no_new_errors
    pub fn passed(&self) -> bool {
        self.new_errors.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Diagnostic, ErrorCode, FindingCode, UnmappedDiagnostic};
    use std::collections::BTreeMap;

    fn rel(s: &str) -> RelPath {
        RelPath::parse(s).unwrap()
    }

    #[test]
    fn passed_iff_no_new_errors() {
        assert!(Verdict::from_diagnostics(Diagnostics::default()).passed());
        let d = Diagnostics {
            errors: vec![Diagnostic {
                code: ErrorCode::parse("E0599").unwrap(),
                path: rel("a.rs"),
                line: 1,
            }],
            unmapped: vec![],
        };
        assert!(!Verdict::from_diagnostics(d).passed());
    }

    #[test]
    fn from_baseline_splits_codes() {
        let e = |code: FindingCode, line| Finding {
            code,
            message: "m".to_owned(),
            path: rel("a.rs"),
            line,
        };
        let rustc = FindingCode::Rustc(ErrorCode::parse("E0308").unwrap());
        let ra = FindingCode::Analyzer("syntax-error".to_owned());
        let checked = ScopedFindings {
            files: vec![rel("a.rs")],
            findings: vec![e(rustc.clone(), 2), e(ra.clone(), 4)],
            ..Default::default()
        };
        let v = Verdict::from_baseline(&[], &checked);
        assert!(!v.passed());
        assert_eq!(
            v.new_errors.errors,
            vec![Diagnostic {
                code: ErrorCode::parse("E0308").unwrap(),
                path: rel("a.rs"),
                line: 2,
            }]
        );
        assert_eq!(
            v.new_errors.unmapped,
            vec![UnmappedDiagnostic {
                ra_code: "syntax-error".to_owned(),
                path: rel("a.rs"),
                line: 4,
            }]
        );
    }

    #[test]
    fn from_baseline_carries_the_observed_unseen() {
        let none = ScopedFindings::default();
        assert!(Verdict::from_baseline(&[], &none).unseen.is_empty());
        let seen = ScopedFindings {
            unseen_at: BTreeMap::from([(UnseenClass::UnexpandedMacro, BTreeSet::new())]),
            ..Default::default()
        };
        let v = Verdict::from_baseline(&[], &seen);
        assert_eq!(v.unseen, BTreeSet::from([UnseenClass::UnexpandedMacro]));
        assert!(v.passed());
    }

    fn e0599(path: &str, line: u32, message: &str) -> Finding {
        Finding {
            code: FindingCode::Rustc(ErrorCode::parse("E0599").unwrap()),
            message: message.to_owned(),
            path: rel(path),
            line,
        }
    }

    #[test]
    fn of_level_with_nothing_below_is_the_check() {
        let baseline = vec![e0599("a.rs", 1, "old"), e0599("b.rs", 2, "kept")];
        let mut level = ScopedFindings {
            files: vec![rel("a.rs"), rel("b.rs")],
            // An added copy of an existing error, the error b kept, and a new one.
            findings: vec![
                e0599("a.rs", 1, "old"),
                e0599("a.rs", 1, "old"),
                e0599("a.rs", 5, "new"),
                e0599("b.rs", 2, "kept"),
            ],
            unseen_at: BTreeMap::from([
                (UnseenClass::UnexpandedMacro, BTreeSet::from([rel("b.rs")])),
                (UnseenClass::UnresolvedPath, BTreeSet::new()),
            ]),
        };
        level.findings.sort();
        let (v, found) = Verdict::of_level(
            &baseline,
            &ScopedFindings::default(),
            &level,
            &[],
            &level.files,
        );
        assert_eq!(found, level);
        assert_eq!(v, Verdict::from_baseline(&baseline, &level));
        assert_eq!(v, Verdict::of_check(&baseline, &level));
        assert_eq!(v.new_errors.errors.len(), 2, "{v:?}");
        assert_eq!(
            v.unseen,
            BTreeSet::from([UnseenClass::UnexpandedMacro, UnseenClass::UnresolvedPath])
        );
    }

    #[test]
    fn of_level_composes_then_keeps_the_files_of_the_check() {
        // The baseline has no error. Below: an error on b, and a class on c. The level diagnoses
        // a, with an error. The check covers a only.
        let below = ScopedFindings {
            files: vec![rel("b.rs"), rel("c.rs")],
            findings: vec![e0599("b.rs", 1, "below")],
            unseen_at: BTreeMap::from([(
                UnseenClass::UnexpandedMacro,
                BTreeSet::from([rel("c.rs")]),
            )]),
        };
        let level = ScopedFindings {
            files: vec![rel("a.rs")],
            findings: vec![e0599("a.rs", 3, "top")],
            ..Default::default()
        };
        let files = [rel("a.rs")];
        let (v, found) = Verdict::of_level(&[], &below, &level, &[], &files);
        assert_eq!(found, level.on_top_of(&below, &[]).within(&files));
        assert_eq!(found.files, files);
        assert_eq!(found.findings, vec![e0599("a.rs", 3, "top")]);
        assert!(found.unseen_at.is_empty(), "{:?}", found.unseen_at);
        assert_eq!(v, Verdict::from_baseline(&[], &found));
        assert_eq!(
            v.new_errors.errors,
            vec![Diagnostic {
                code: ErrorCode::parse("E0599").unwrap(),
                path: rel("a.rs"),
                line: 3,
            }]
        );
        assert!(v.unseen.is_empty());
    }
}
