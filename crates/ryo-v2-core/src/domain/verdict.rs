//! The outcome of verifying edits.

use super::baseline::new_findings;
use super::{Diagnostics, Finding, ScopedFindings, UnseenClass};
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
    /// - C2: `unseen` is the `unseen` of `checked`.
    ///   tested: domain::verdict::tests::from_baseline_carries_the_observed_unseen
    pub fn from_baseline(baseline: &[Finding], checked: &ScopedFindings) -> Self {
        Self {
            unseen: checked.unseen.clone(),
            ..Self::from_diagnostics(Diagnostics::from_findings(&new_findings(baseline, checked)))
        }
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
    use crate::domain::{Diagnostic, ErrorCode, FindingCode, RelPath, UnmappedDiagnostic};

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
            unseen: BTreeSet::from([UnseenClass::UnexpandedMacro]),
            ..Default::default()
        };
        let v = Verdict::from_baseline(&[], &seen);
        assert_eq!(v.unseen, BTreeSet::from([UnseenClass::UnexpandedMacro]));
        assert!(v.passed());
    }
}
