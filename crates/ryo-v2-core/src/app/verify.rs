//! The verify use case.

use crate::app::ports::{Analyzer, AnalyzerError};
use crate::domain::{Diagnostics, Edit, Scope, ScopedFindings, Verdict};

/// fork → apply → diagnostics → discard。discard は apply / diagnostics が Err でも必ず呼ぶ。
/// 返す Err の優先: apply の Err > diagnostics の Err > discard の Err。edits が空なら analyzer を呼ばず passed。
/// 成功時の Verdict は、scope 内の全 Error と baseline の差 (`domain::baseline`) から作る。
///
/// # Contract
/// - C1: the verdict of edits that apply and diagnose is `Verdict::of_check` of the findings
///   (`Verdict::of_level` with nothing below, nothing deleted and their own files), as a check's
///   is.
///   tested: app::verify::tests::verify_forms_its_verdict_as_a_check_does
pub fn verify<A: Analyzer>(
    analyzer: &A,
    session: &mut A::Session,
    edits: &[Edit],
    scope: Scope,
) -> Result<Verdict, AnalyzerError> {
    if edits.is_empty() {
        return Ok(Verdict::from_diagnostics(Diagnostics::default()));
    }
    let mut fork = analyzer.fork(session)?;
    let applied = analyzer.apply(session, &mut fork, edits);
    let checked = match applied {
        Ok(()) => Some(analyzer.diagnostics(session, &fork, scope)),
        Err(_) => None,
    };
    let discarded = analyzer.discard(session, fork);
    applied?;
    let checked = checked.unwrap_or_else(|| Ok(ScopedFindings::default()))?;
    discarded?;
    // The baseline is load-time state; taken after discard.
    Ok(Verdict::of_check(&analyzer.baseline(session), &checked))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Diagnostic, ErrorCode, Finding, FindingCode, RelPath, UnseenClass};
    use std::cell::Cell;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    struct Fake {
        fail_apply: bool,
        baseline: Vec<Finding>,
        files: Vec<RelPath>,
        findings: Vec<Finding>,
        unseen_at: BTreeMap<UnseenClass, BTreeSet<RelPath>>,
        discards: Cell<u32>,
    }

    impl Analyzer for Fake {
        type Session = ();
        type Fork = ();
        fn load(&self, _: &Path) -> Result<(), AnalyzerError> {
            Ok(())
        }
        fn baseline(&self, _: &()) -> Vec<Finding> {
            self.baseline.clone()
        }
        fn fork(&self, _: &mut ()) -> Result<(), AnalyzerError> {
            Ok(())
        }
        fn apply(&self, _: &mut (), _: &mut (), _: &[Edit]) -> Result<(), AnalyzerError> {
            if self.fail_apply {
                Err(AnalyzerError::Cancelled)
            } else {
                Ok(())
            }
        }
        fn diagnostics(&self, _: &(), _: &(), _: Scope) -> Result<ScopedFindings, AnalyzerError> {
            Ok(ScopedFindings {
                files: self.files.clone(),
                findings: self.findings.clone(),
                unseen_at: self.unseen_at.clone(),
            })
        }
        fn discard(&self, _: &mut (), _: ()) -> Result<(), AnalyzerError> {
            self.discards.set(self.discards.get() + 1);
            Ok(())
        }
    }

    fn edit() -> Vec<Edit> {
        vec![Edit::write(RelPath::parse("a.rs").unwrap(), "x")]
    }

    fn finding(code: &str, msg: &str, line: u32) -> Finding {
        Finding {
            code: FindingCode::Rustc(ErrorCode::parse(code).unwrap()),
            message: msg.to_owned(),
            path: RelPath::parse("a.rs").unwrap(),
            line,
        }
    }

    fn fake(fail_apply: bool, baseline: Vec<Finding>, findings: Vec<Finding>) -> Fake {
        Fake {
            fail_apply,
            baseline,
            files: vec![RelPath::parse("a.rs").unwrap()],
            findings,
            unseen_at: BTreeMap::new(),
            discards: Cell::new(0),
        }
    }

    #[test]
    fn verify_forms_its_verdict_as_a_check_does() {
        // An added copy of a baseline error, the other baseline error moved, and classes, one
        // with its file and one with none.
        let mut a = fake(
            false,
            vec![finding("E0599", "m", 3), finding("E0308", "t", 7)],
            vec![
                finding("E0599", "m", 3),
                finding("E0599", "m", 3),
                finding("E0308", "t", 9),
            ],
        );
        a.unseen_at = BTreeMap::from([
            (
                UnseenClass::UnexpandedMacro,
                BTreeSet::from([RelPath::parse("a.rs").unwrap()]),
            ),
            (UnseenClass::UnresolvedPath, BTreeSet::new()),
        ]);
        let v = verify(&a, &mut (), &edit(), Scope::Affected).unwrap();
        let findings = a.diagnostics(&(), &(), Scope::Affected).unwrap();
        let (want, _) = Verdict::of_level(
            &a.baseline,
            &ScopedFindings::default(),
            &findings,
            &[],
            &findings.files,
        );
        assert_eq!(v, want);
        assert_eq!(v.new_errors.errors.len(), 1, "{v:?}");
        assert_eq!(
            v.unseen,
            BTreeSet::from([UnseenClass::UnexpandedMacro, UnseenClass::UnresolvedPath])
        );
    }

    #[test]
    fn verify_discards_fork_when_apply_fails() {
        let a = fake(true, vec![], vec![]);
        let r = verify(&a, &mut (), &edit(), Scope::Affected);
        assert!(matches!(r, Err(AnalyzerError::Cancelled)));
        assert_eq!(a.discards.get(), 1);
    }

    #[test]
    fn verify_fails_on_new_error() {
        let f = finding("E0599", "m", 3);
        let a = fake(false, vec![], vec![f]);
        let v = verify(&a, &mut (), &edit(), Scope::Affected).unwrap();
        assert!(!v.passed());
        assert_eq!(
            v.new_errors.errors,
            vec![Diagnostic {
                code: ErrorCode::parse("E0599").unwrap(),
                path: RelPath::parse("a.rs").unwrap(),
                line: 3,
            }]
        );
        assert_eq!(a.discards.get(), 1);
        // no edits: analyzer untouched, passed
        let v = verify(&a, &mut (), &[], Scope::Affected).unwrap();
        assert!(v.passed());
        assert_eq!(a.discards.get(), 1);
    }

    #[test]
    fn verify_subtracts_a_baseline_with_errors() {
        let a = fake(
            false,
            vec![finding("E0599", "m", 3)],
            vec![finding("E0599", "m", 5), finding("E0308", "t", 7)],
        );
        let v = verify(&a, &mut (), &edit(), Scope::Affected).unwrap();
        assert!(!v.passed());
        assert_eq!(
            v.new_errors.errors,
            vec![Diagnostic {
                code: ErrorCode::parse("E0308").unwrap(),
                path: RelPath::parse("a.rs").unwrap(),
                line: 7,
            }]
        );
        assert!(v.new_errors.unmapped.is_empty());
    }
}
