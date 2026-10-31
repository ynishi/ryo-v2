//! Error groups, the comparison of expected and actual diagnostics, and the
//! per-group counts the ledger is written from.

use std::collections::{BTreeMap, BTreeSet};

use ryo_v2_core::domain::{Diagnostic, Diagnostics, UnmappedDiagnostic};
use serde_json::{Value, json};

/// A class of rustc errors. A case belongs to the group of the error it is
/// written to provoke; `Valid` is a case that must produce none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Group {
    /// E0432, E0433.
    UnresolvedImport,
    /// E0425.
    UnresolvedName,
    /// E0412, E0422.
    UnresolvedType,
    /// E0599.
    NoMethod,
    /// E0308, E0369.
    TypeMismatch,
    /// E0277.
    TraitBound,
    /// E0061, E0107.
    Arity,
    /// E0609, E0560, E0063, E0559.
    Field,
    /// E0382, E0499, E0502, E0505, E0506.
    Borrow,
    /// E0603, E0616, E0624.
    Privacy,
    /// E0080, E0015.
    ConstEval,
    /// A reference to a cfg-disabled item. The group is the cause, not the
    /// code: the codes overlap other groups.
    Cfg,
    /// No error expected.
    Valid,
}

impl Group {
    /// Every group, in declaration order.
    pub const ALL: [Group; 13] = [
        Group::UnresolvedImport,
        Group::UnresolvedName,
        Group::UnresolvedType,
        Group::NoMethod,
        Group::TypeMismatch,
        Group::TraitBound,
        Group::Arity,
        Group::Field,
        Group::Borrow,
        Group::Privacy,
        Group::ConstEval,
        Group::Cfg,
        Group::Valid,
    ];

    /// The groups the H7 recall is taken over.
    pub const PRIMARY: [Group; 5] = [
        Group::UnresolvedImport,
        Group::UnresolvedName,
        Group::NoMethod,
        Group::TypeMismatch,
        Group::TraitBound,
    ];

    /// The id used in case files and on the command line.
    pub fn id(self) -> &'static str {
        match self {
            Group::UnresolvedImport => "unresolved-import",
            Group::UnresolvedName => "unresolved-name",
            Group::UnresolvedType => "unresolved-type",
            Group::NoMethod => "no-method",
            Group::TypeMismatch => "type-mismatch",
            Group::TraitBound => "trait-bound",
            Group::Arity => "arity",
            Group::Field => "field",
            Group::Borrow => "borrow",
            Group::Privacy => "privacy",
            Group::ConstEval => "const-eval",
            Group::Cfg => "cfg",
            Group::Valid => "valid",
        }
    }

    /// The group with this id, if any.
    pub fn parse(s: &str) -> Option<Group> {
        Group::ALL.into_iter().find(|g| g.id() == s)
    }

    /// The rustc error codes that belong to the group.
    pub fn codes(self) -> &'static [&'static str] {
        match self {
            Group::UnresolvedImport => &["E0432", "E0433"],
            Group::UnresolvedName => &["E0425"],
            Group::UnresolvedType => &["E0412", "E0422"],
            Group::NoMethod => &["E0599"],
            Group::TypeMismatch => &["E0308", "E0369"],
            Group::TraitBound => &["E0277"],
            Group::Arity => &["E0061", "E0107"],
            Group::Field => &["E0609", "E0560", "E0063", "E0559"],
            Group::Borrow => &["E0382", "E0499", "E0502", "E0505", "E0506"],
            Group::Privacy => &["E0603", "E0616", "E0624"],
            Group::ConstEval => &["E0080", "E0015"],
            Group::Cfg => &["E0425", "E0432", "E0433", "E0412", "E0599"],
            Group::Valid => &[],
        }
    }
}

/// What comparing one case produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The verifier reported exactly the expected diagnostics.
    Pass,
    /// It did not.
    Fail {
        /// Expected but not reported (false negatives).
        missed: Vec<Diagnostic>,
        /// Reported but not expected (false positives).
        extra: Vec<Diagnostic>,
        /// Reported with a rust-analyzer code that has no rustc code (false positives).
        unmapped: Vec<UnmappedDiagnostic>,
    },
    /// The case was not compared; the string says why.
    Skip(String),
}

/// Set comparison (duplicates count once). `missed` = expected - actual errors,
/// `extra` = actual errors - expected, `unmapped` = the actual unmapped ones.
/// All three empty is a pass.
pub fn compare(expected: &[Diagnostic], actual: &Diagnostics) -> Outcome {
    let exp: BTreeSet<&Diagnostic> = expected.iter().collect();
    let act: BTreeSet<&Diagnostic> = actual.errors.iter().collect();
    let missed: Vec<Diagnostic> = exp.difference(&act).map(|d| (*d).clone()).collect();
    let extra: Vec<Diagnostic> = act.difference(&exp).map(|d| (*d).clone()).collect();
    let unmapped: Vec<UnmappedDiagnostic> = actual
        .unmapped
        .iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .cloned()
        .collect();
    if missed.is_empty() && extra.is_empty() && unmapped.is_empty() {
        Outcome::Pass
    } else {
        Outcome::Fail {
            missed,
            extra,
            unmapped,
        }
    }
}

/// The result of one case.
#[derive(Debug, Clone)]
pub struct CaseOutcome {
    /// The case id.
    pub id: String,
    /// The group of the case.
    pub group: Group,
    /// What the comparison produced.
    pub outcome: Outcome,
    /// Milliseconds the in-memory check took, when it ran.
    pub check_ms: Option<u64>,
}

/// Counts of one group.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Cases in the group, skipped ones included.
    pub cases: usize,
    /// Passes.
    pub pass: usize,
    /// Failures with a missed diagnostic.
    pub fn_: usize,
    /// Failures with an extra or unmapped diagnostic (a failure can count in both).
    pub fp: usize,
    /// Skipped cases.
    pub skip: usize,
}

/// Every outcome of a run, and what was left out of it.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// One per case that was considered.
    pub outcomes: Vec<CaseOutcome>,
    /// `(case id, "only" | "limit")` for cases filtered out of the run.
    pub excluded: Vec<(String, String)>,
}

impl Report {
    /// Counts per group; only groups that have a case appear.
    pub fn counts(&self) -> BTreeMap<Group, Counts> {
        let mut map: BTreeMap<Group, Counts> = BTreeMap::new();
        for o in &self.outcomes {
            let c = map.entry(o.group).or_default();
            c.cases += 1;
            match &o.outcome {
                Outcome::Pass => c.pass += 1,
                Outcome::Skip(_) => c.skip += 1,
                Outcome::Fail {
                    missed,
                    extra,
                    unmapped,
                } => {
                    if !missed.is_empty() {
                        c.fn_ += 1;
                    }
                    if !extra.is_empty() || !unmapped.is_empty() {
                        c.fp += 1;
                    }
                }
            }
        }
        map
    }

    /// `(pass, pass + fn_)` over the primary groups; `None` when the
    /// denominator is 0.
    pub fn primary_recall(&self) -> Option<(usize, usize)> {
        let counts = self.counts();
        let (mut pass, mut denom) = (0, 0);
        for g in Group::PRIMARY {
            if let Some(c) = counts.get(&g) {
                pass += c.pass;
                denom += c.pass + c.fn_;
            }
        }
        (denom > 0).then_some((pass, denom))
    }

    /// Every outcome as JSON, for `RYO_V2_ORACLE_REPORT`.
    pub fn to_json(&self) -> Value {
        let diag = |d: &Diagnostic| json!({"code": d.code.as_str(), "path": d.path.as_str(), "line": d.line});
        let outcomes: Vec<Value> = self
            .outcomes
            .iter()
            .map(|o| {
                let mut v = json!({"id": o.id, "group": o.group.id(), "check_ms": o.check_ms});
                match &o.outcome {
                    Outcome::Pass => v["outcome"] = json!("pass"),
                    Outcome::Skip(why) => {
                        v["outcome"] = json!("skip");
                        v["reason"] = json!(why);
                    }
                    Outcome::Fail {
                        missed,
                        extra,
                        unmapped,
                    } => {
                        v["outcome"] = json!("fail");
                        v["missed"] = json!(missed.iter().map(diag).collect::<Vec<_>>());
                        v["extra"] = json!(extra.iter().map(diag).collect::<Vec<_>>());
                        v["unmapped"] = json!(
                            unmapped
                                .iter()
                                .map(|u| json!({"ra_code": u.ra_code, "path": u.path.as_str(), "line": u.line}))
                                .collect::<Vec<_>>()
                        );
                    }
                }
                v
            })
            .collect();
        let counts: serde_json::Map<String, Value> = self
            .counts()
            .into_iter()
            .map(|(g, c)| {
                (
                    g.id().to_string(),
                    json!({"cases": c.cases, "pass": c.pass, "fn": c.fn_, "fp": c.fp, "skip": c.skip}),
                )
            })
            .collect();
        let excluded: Vec<Value> = self
            .excluded
            .iter()
            .map(|(id, why)| json!({"id": id, "why": why}))
            .collect();
        let recall = self
            .primary_recall()
            .map(|(p, n)| json!({"pass": p, "of": n}));
        json!({"outcomes": outcomes, "counts": counts, "primary_recall": recall, "excluded": excluded})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryo_v2_core::domain::{ErrorCode, RelPath};

    fn d(code: &str, path: &str, line: u32) -> Diagnostic {
        Diagnostic {
            code: ErrorCode::parse(code).unwrap(),
            path: RelPath::parse(path).unwrap(),
            line,
        }
    }

    fn actual(errors: Vec<Diagnostic>) -> Diagnostics {
        Diagnostics {
            errors,
            unmapped: Vec::new(),
        }
    }

    #[test]
    fn compare_pass_when_sets_equal() {
        let e = vec![d("E0599", "user/src/lib.rs", 20)];
        assert_eq!(compare(&e, &actual(e.clone())), Outcome::Pass);
        // duplicates count once
        let dup = vec![e[0].clone(), e[0].clone()];
        assert_eq!(compare(&e, &actual(dup)), Outcome::Pass);
        assert_eq!(compare(&[], &Diagnostics::default()), Outcome::Pass);
    }

    #[test]
    fn compare_reports_missed_and_extra() {
        let e = vec![d("E0599", "user/src/lib.rs", 20)];
        let a = actual(vec![d("E0599", "user/src/lib.rs", 21)]);
        assert_eq!(
            compare(&e, &a),
            Outcome::Fail {
                missed: vec![d("E0599", "user/src/lib.rs", 20)],
                extra: vec![d("E0599", "user/src/lib.rs", 21)],
                unmapped: vec![],
            }
        );
    }

    #[test]
    fn compare_counts_unmapped_as_fail() {
        let a = Diagnostics {
            errors: vec![],
            unmapped: vec![UnmappedDiagnostic {
                ra_code: "syntax-error".into(),
                path: RelPath::parse("lib/src/lib.rs").unwrap(),
                line: 3,
            }],
        };
        match compare(&[], &a) {
            Outcome::Fail {
                missed,
                extra,
                unmapped,
            } => {
                assert!(missed.is_empty() && extra.is_empty());
                assert_eq!(unmapped.len(), 1);
            }
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    #[test]
    fn group_parse_round_trips_all_ids() {
        for g in Group::ALL {
            assert_eq!(Group::parse(g.id()), Some(g));
        }
        assert_eq!(Group::parse("nope"), None);
        assert_eq!(Group::ALL.len(), 13);
        assert!(Group::Valid.codes().is_empty());
        assert!(Group::PRIMARY.iter().all(|g| Group::ALL.contains(g)));
    }

    fn outcome(id: &str, group: Group, outcome: Outcome) -> CaseOutcome {
        CaseOutcome {
            id: id.into(),
            group,
            outcome,
            check_ms: None,
        }
    }

    #[test]
    fn report_counts_and_primary_recall() {
        let missed = d("E0599", "a/src/lib.rs", 1);
        let extra = d("E0308", "a/src/lib.rs", 2);
        let report = Report {
            outcomes: vec![
                outcome("a", Group::NoMethod, Outcome::Pass),
                outcome(
                    "b",
                    Group::NoMethod,
                    Outcome::Fail {
                        missed: vec![missed],
                        extra: vec![extra.clone()],
                        unmapped: vec![],
                    },
                ),
                outcome("c", Group::TypeMismatch, Outcome::Skip("why".into())),
                outcome(
                    "d",
                    Group::Valid,
                    Outcome::Fail {
                        missed: vec![],
                        extra: vec![extra],
                        unmapped: vec![],
                    },
                ),
                outcome("e", Group::Privacy, Outcome::Pass),
            ],
            excluded: vec![],
        };
        let counts = report.counts();
        assert_eq!(
            counts[&Group::NoMethod],
            Counts {
                cases: 2,
                pass: 1,
                fn_: 1,
                fp: 1,
                skip: 0
            }
        );
        assert_eq!(counts[&Group::TypeMismatch].skip, 1);
        assert_eq!(counts[&Group::TypeMismatch].cases, 1);
        assert_eq!(counts[&Group::Valid].fp, 1);
        // Privacy and Valid are not primary: 1 pass of (1 pass + 1 fn).
        assert_eq!(report.primary_recall(), Some((1, 2)));
        assert_eq!(Report::default().primary_recall(), None);
        assert_eq!(report.to_json()["primary_recall"]["of"], 2);
    }
}
