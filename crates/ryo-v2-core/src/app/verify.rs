//! The verify use case.

use crate::app::ports::{Analyzer, AnalyzerError};
use crate::domain::{Diagnostics, Edit, Scope, Verdict};

/// fork → apply → diagnostics → discard。discard は apply / diagnostics が Err でも必ず呼ぶ。
/// 返す Err の優先: apply の Err > diagnostics の Err > discard の Err。edits が空なら analyzer を呼ばず passed。
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
    let diags = checked.unwrap_or_else(|| Ok(Diagnostics::default()))?;
    discarded?;
    Ok(Verdict::from_diagnostics(diags))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Diagnostic, ErrorCode, RelPath};
    use std::cell::Cell;
    use std::path::Path;

    struct Fake {
        fail_apply: bool,
        errors: Vec<Diagnostic>,
        discards: Cell<u32>,
    }

    impl Analyzer for Fake {
        type Session = ();
        type Fork = ();
        fn load(&self, _: &Path) -> Result<(), AnalyzerError> {
            Ok(())
        }
        fn baseline(&self, _: &()) -> Diagnostics {
            Diagnostics::default()
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
        fn diagnostics(&self, _: &(), _: &(), _: Scope) -> Result<Diagnostics, AnalyzerError> {
            Ok(Diagnostics {
                errors: self.errors.clone(),
                unmapped: vec![],
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

    #[test]
    fn verify_discards_fork_when_apply_fails() {
        let a = Fake {
            fail_apply: true,
            errors: vec![],
            discards: Cell::new(0),
        };
        let r = verify(&a, &mut (), &edit(), Scope::Affected);
        assert!(matches!(r, Err(AnalyzerError::Cancelled)));
        assert_eq!(a.discards.get(), 1);
    }

    #[test]
    fn verify_fails_on_new_error() {
        let d = Diagnostic {
            code: ErrorCode::parse("E0599").unwrap(),
            path: RelPath::parse("a.rs").unwrap(),
            line: 3,
        };
        let a = Fake {
            fail_apply: false,
            errors: vec![d.clone()],
            discards: Cell::new(0),
        };
        let v = verify(&a, &mut (), &edit(), Scope::Affected).unwrap();
        assert!(!v.passed);
        assert_eq!(v.new_errors.errors, vec![d]);
        assert_eq!(a.discards.get(), 1);
        // no edits: analyzer untouched, passed
        let v = verify(&a, &mut (), &[], Scope::Affected).unwrap();
        assert!(v.passed);
        assert_eq!(a.discards.get(), 1);
    }
}
