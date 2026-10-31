//! Error groups, the comparison of expected and actual diagnostics, and the
//! per-group counts the ledger is written from.

use std::collections::{BTreeMap, BTreeSet};

use ryo_v2_core::domain::{Diagnostic, Diagnostics, UnmappedDiagnostic};
use serde_json::{Value, json};

use crate::case::GapTag;

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
    /// An error inside `#[cfg(test)]` code or a `tests/` file (E0425, E0433, E0308, E0599). A
    /// cause: the verifier sees it only with `cfg(test)` enabled.
    CfgTest,
    /// A derive-generated method is missing (E0599 at the use site). A cause.
    MacroDerive,
    /// A type error inside the body of an attribute-macro item (E0308). A cause.
    MacroAttr,
    /// An error inside a `macro_rules!` expansion (E0599, E0308). A cause.
    MacroRules,
    /// An edit inside the tokens of a macro call (E0599, E0308). A cause.
    MacroTokens,
    /// An edit of a proc-macro crate; the correct outcome is a false negative
    /// (E0599). A cause.
    ProcMacroEdit,
    /// No error expected.
    Valid,
}

impl Group {
    /// Every group, in declaration order.
    pub const ALL: [Group; 19] = [
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
        Group::CfgTest,
        Group::MacroDerive,
        Group::MacroAttr,
        Group::MacroRules,
        Group::MacroTokens,
        Group::ProcMacroEdit,
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
            Group::CfgTest => "cfg-test",
            Group::MacroDerive => "macro-derive",
            Group::MacroAttr => "macro-attr",
            Group::MacroRules => "macro-rules",
            Group::MacroTokens => "macro-tokens",
            Group::ProcMacroEdit => "proc-macro-edit",
            Group::Valid => "valid",
        }
    }

    /// The group is a cause, not a set of codes (its codes overlap other
    /// groups): Cfg, CfgTest and the 5 macro groups. `breakage::group_for` never maps a
    /// code to one.
    pub fn is_cause(self) -> bool {
        matches!(
            self,
            Group::Cfg
                | Group::CfgTest
                | Group::MacroDerive
                | Group::MacroAttr
                | Group::MacroRules
                | Group::MacroTokens
                | Group::ProcMacroEdit
        )
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
            Group::CfgTest => &["E0425", "E0433", "E0308", "E0599"],
            Group::MacroDerive => &["E0599"],
            Group::MacroAttr => &["E0308"],
            Group::MacroRules => &["E0599", "E0308"],
            Group::MacroTokens => &["E0599", "E0308"],
            Group::ProcMacroEdit => &["E0599"],
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
    /// The expected diagnostics (empty for Skip).
    pub expected: Vec<Diagnostic>,
    /// What the comparison produced.
    pub outcome: Outcome,
    /// Milliseconds the in-memory check took, when it ran.
    pub check_ms: Option<u64>,
}

/// Counts of one group. "compared" = not Skip.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Cases in the group, skipped ones included.
    pub cases: usize,
    /// Skipped cases.
    pub skip: usize,
    /// Compared cases with at least one expected diagnostic whose code is in
    /// `group.codes()`: the recall denominator. Always 0 for Valid.
    pub of: usize,
    /// Of `of`, the cases where no expected diagnostic with a group code was
    /// missed (extra and unmapped diagnostics allowed).
    pub hit: usize,
    /// `of - hit`.
    pub fn_: usize,
    /// Compared cases whose outcome is `Outcome::Pass` (exact set equality).
    pub exact: usize,
    /// Compared cases with at least one extra or unmapped diagnostic, any code.
    pub fp: usize,
    /// Distinct expected diagnostics with a group code that were reported
    /// (precision numerator).
    pub tp_diags: usize,
    /// Distinct reported diagnostics with a group code that were not expected.
    pub fp_diags: usize,
}

impl Counts {
    /// `hit / of`, `None` when `of == 0`.
    pub fn recall(&self) -> Option<f64> {
        (self.of > 0).then(|| self.hit as f64 / self.of as f64)
    }

    /// `tp_diags / (tp_diags + fp_diags)`, `None` when that is 0.
    pub fn precision(&self) -> Option<f64> {
        let n = self.tp_diags + self.fp_diags;
        (n > 0).then(|| self.tp_diags as f64 / n as f64)
    }
}

/// FN / FP of one code outside the group of the case it appeared in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OtherCounts {
    /// Expected, not reported.
    pub missed: usize,
    /// Reported, not expected (unmapped diagnostics included, under "ra:<code>").
    pub extra: usize,
    /// Cases with at least one of either.
    pub cases: usize,
}

/// Whether `d` is tagged with one of `rows` in case `id`.
fn is_tagged(tags: &[GapTag], rows: &[&str], id: &str, d: &Diagnostic) -> bool {
    tags.iter()
        .any(|t| t.case == id && t.diagnostic == *d && rows.contains(&t.row.as_str()))
}

fn in_codes(group: Group, d: &Diagnostic) -> bool {
    group.codes().contains(&d.code.as_str())
}

/// The distinct diagnostics of `items` with a code of `group`.
fn group_set(group: Group, items: &[Diagnostic]) -> BTreeSet<&Diagnostic> {
    items.iter().filter(|d| in_codes(group, d)).collect()
}

/// `(missed, extra, unmapped)` of an outcome; empty unless it is a `Fail`.
fn parts(o: &CaseOutcome) -> (&[Diagnostic], &[Diagnostic], &[UnmappedDiagnostic]) {
    match &o.outcome {
        Outcome::Fail {
            missed,
            extra,
            unmapped,
        } => (missed, extra, unmapped),
        _ => (&[], &[], &[]),
    }
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
    ///
    /// Per compared case (not Skip) of group `g`, with `codes = g.codes()` and
    /// sets (a duplicate counts once, as [`compare`] does):
    /// - `exp_g` is the expected diagnostics with a code in `codes`, `missed_g`
    ///   the missed ones and `extra_g` the extra ones with a code in `codes`.
    /// - `of` grows iff `exp_g` is non-empty, and then `hit` grows iff
    ///   `missed_g` is empty (extra and unmapped diagnostics are allowed).
    /// - `exact` grows iff the outcome is `Pass`; `fp` grows iff there is an
    ///   extra or unmapped diagnostic of any code.
    /// - `tp_diags += |exp_g| - |missed_g|` and `fp_diags += |extra_g|`.
    ///
    /// Everything with a code outside `codes` (missed, extra) and every
    /// unmapped diagnostic goes to [`Report::other_codes`]. Valid has no codes,
    /// so its `of` is 0 and its floor uses `exact`.
    pub fn counts(&self) -> BTreeMap<Group, Counts> {
        let mut map: BTreeMap<Group, Counts> = BTreeMap::new();
        for o in &self.outcomes {
            let c = map.entry(o.group).or_default();
            c.cases += 1;
            if matches!(o.outcome, Outcome::Skip(_)) {
                c.skip += 1;
                continue;
            }
            let (missed, extra, unmapped) = parts(o);
            let exp_g = group_set(o.group, &o.expected);
            let missed_g = group_set(o.group, missed);
            let extra_g = group_set(o.group, extra);
            if !exp_g.is_empty() {
                c.of += 1;
                if missed_g.is_empty() {
                    c.hit += 1;
                } else {
                    c.fn_ += 1;
                }
            }
            if o.outcome == Outcome::Pass {
                c.exact += 1;
            }
            if !extra.is_empty() || !unmapped.is_empty() {
                c.fp += 1;
            }
            c.tp_diags += exp_g.len() - exp_g.intersection(&missed_g).count();
            c.fp_diags += extra_g.len();
        }
        map
    }

    /// FN / FP per code outside the group of the case it appeared in. Keyed
    /// by E-code, or `ra:<ra_code>` for an unmapped diagnostic (always counted
    /// as extra). `cases` counts the compared cases that had the code.
    pub fn other_codes(&self) -> BTreeMap<String, OtherCounts> {
        let mut map: BTreeMap<String, OtherCounts> = BTreeMap::new();
        for o in &self.outcomes {
            if matches!(o.outcome, Outcome::Skip(_)) {
                continue;
            }
            let (missed, extra, unmapped) = parts(o);
            let mut seen: BTreeSet<String> = BTreeSet::new();
            for d in missed.iter().collect::<BTreeSet<_>>() {
                if !in_codes(o.group, d) {
                    let key = d.code.as_str().to_string();
                    map.entry(key.clone()).or_default().missed += 1;
                    seen.insert(key);
                }
            }
            for d in extra.iter().collect::<BTreeSet<_>>() {
                if !in_codes(o.group, d) {
                    let key = d.code.as_str().to_string();
                    map.entry(key.clone()).or_default().extra += 1;
                    seen.insert(key);
                }
            }
            for u in unmapped {
                let key = format!("ra:{}", u.ra_code);
                map.entry(key.clone()).or_default().extra += 1;
                seen.insert(key);
            }
            for key in seen {
                map.entry(key).or_default().cases += 1;
            }
        }
        map
    }

    /// `(hit, of)` summed over [`Group::PRIMARY`]; `None` when `of == 0`.
    pub fn primary_recall(&self) -> Option<(usize, usize)> {
        let counts = self.counts();
        let (mut hit, mut of) = (0, 0);
        for g in Group::PRIMARY {
            if let Some(c) = counts.get(&g) {
                hit += c.hit;
                of += c.of;
            }
        }
        (of > 0).then_some((hit, of))
    }

    /// Same as [`Report::primary_recall`], after removing from each case's
    /// expected set the diagnostics tagged with one of `rows` (a case whose
    /// group-code expected set becomes empty leaves the denominator).
    pub fn primary_recall_without(&self, tags: &[GapTag], rows: &[&str]) -> Option<(usize, usize)> {
        let (mut hit, mut of) = (0, 0);
        for o in &self.outcomes {
            if !Group::PRIMARY.contains(&o.group) || matches!(o.outcome, Outcome::Skip(_)) {
                continue;
            }
            let (missed, _, _) = parts(o);
            let kept = |d: &&Diagnostic| !is_tagged(tags, rows, &o.id, d);
            let exp_g: Vec<&Diagnostic> = o
                .expected
                .iter()
                .filter(|d| in_codes(o.group, d))
                .filter(kept)
                .collect();
            if exp_g.is_empty() {
                continue;
            }
            of += 1;
            let any_missed = missed
                .iter()
                .filter(|d| in_codes(o.group, d))
                .any(|d| kept(&d));
            if !any_missed {
                hit += 1;
            }
        }
        (of > 0).then_some((hit, of))
    }

    /// Compared non-Valid cases whose expected set has no code of their group
    /// (printed as MISGROUPED).
    pub fn misgrouped(&self) -> Vec<&CaseOutcome> {
        self.outcomes
            .iter()
            .filter(|o| {
                o.group != Group::Valid
                    && !matches!(o.outcome, Outcome::Skip(_))
                    && !o.expected.iter().any(|d| in_codes(o.group, d))
            })
            .collect()
    }

    /// Every outcome as JSON, for `RYO_V2_ORACLE_REPORT`.
    pub fn to_json(&self) -> Value {
        let diag = |d: &Diagnostic| json!({"code": d.code.as_str(), "path": d.path.as_str(), "line": d.line});
        let outcomes: Vec<Value> = self
            .outcomes
            .iter()
            .map(|o| {
                let mut v = json!({
                    "id": o.id,
                    "group": o.group.id(),
                    "check_ms": o.check_ms,
                    "expected": o.expected.iter().map(diag).collect::<Vec<_>>(),
                });
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
                    json!({
                        "cases": c.cases,
                        "skip": c.skip,
                        "of": c.of,
                        "hit": c.hit,
                        "fn": c.fn_,
                        "exact": c.exact,
                        "fp": c.fp,
                        "tp_diags": c.tp_diags,
                        "fp_diags": c.fp_diags,
                    }),
                )
            })
            .collect();
        let other_codes: serde_json::Map<String, Value> = self
            .other_codes()
            .into_iter()
            .map(|(code, c)| {
                (
                    code,
                    json!({"missed": c.missed, "extra": c.extra, "cases": c.cases}),
                )
            })
            .collect();
        let misgrouped: Vec<&str> = self.misgrouped().iter().map(|o| o.id.as_str()).collect();
        let excluded: Vec<Value> = self
            .excluded
            .iter()
            .map(|(id, why)| json!({"id": id, "why": why}))
            .collect();
        let recall = self
            .primary_recall()
            .map(|(hit, of)| json!({"hit": hit, "of": of}));
        json!({
            "outcomes": outcomes,
            "counts": counts,
            "other_codes": other_codes,
            "primary_recall": recall,
            "primary_recall_without_gb34": Value::Null,
            "misgrouped": misgrouped,
            "excluded": excluded,
        })
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
        assert_eq!(Group::ALL.len(), 19);
        assert!(Group::Valid.codes().is_empty());
        assert!(Group::PRIMARY.iter().all(|g| Group::ALL.contains(g)));
    }

    fn outcome(id: &str, group: Group, expected: Vec<Diagnostic>, outcome: Outcome) -> CaseOutcome {
        CaseOutcome {
            id: id.into(),
            group,
            expected,
            outcome,
            check_ms: None,
        }
    }

    fn fail(missed: Vec<Diagnostic>, extra: Vec<Diagnostic>) -> Outcome {
        Outcome::Fail {
            missed,
            extra,
            unmapped: vec![],
        }
    }

    #[test]
    fn report_counts_and_primary_recall() {
        let hit_diag = d("E0599", "a/src/lib.rs", 10);
        let missed = d("E0599", "a/src/lib.rs", 1);
        let extra = d("E0308", "a/src/lib.rs", 2);
        let report = Report {
            outcomes: vec![
                outcome("a", Group::NoMethod, vec![hit_diag], Outcome::Pass),
                outcome(
                    "b",
                    Group::NoMethod,
                    vec![missed.clone()],
                    fail(vec![missed], vec![extra.clone()]),
                ),
                outcome(
                    "c",
                    Group::TypeMismatch,
                    vec![],
                    Outcome::Skip("why".into()),
                ),
                outcome("d", Group::Valid, vec![], fail(vec![], vec![extra])),
                outcome("e", Group::Privacy, vec![], Outcome::Pass),
            ],
            excluded: vec![],
        };
        let counts = report.counts();
        assert_eq!(
            counts[&Group::NoMethod],
            Counts {
                cases: 2,
                skip: 0,
                of: 2,
                hit: 1,
                fn_: 1,
                exact: 1,
                fp: 1,
                tp_diags: 1,
                fp_diags: 0,
            }
        );
        assert_eq!(counts[&Group::NoMethod].recall(), Some(0.5));
        assert_eq!(counts[&Group::NoMethod].precision(), Some(1.0));
        assert_eq!(counts[&Group::TypeMismatch].skip, 1);
        assert_eq!(counts[&Group::TypeMismatch].cases, 1);
        assert_eq!(counts[&Group::TypeMismatch].of, 0);
        assert_eq!(counts[&Group::Valid].fp, 1);
        // Privacy and Valid are not primary: 1 hit of 2.
        assert_eq!(report.primary_recall(), Some((1, 2)));
        assert_eq!(Report::default().primary_recall(), None);
        let json = report.to_json();
        assert_eq!(json["primary_recall"]["of"], 2);
        assert_eq!(json["counts"]["no-method"]["hit"], 1);
        assert!(json["primary_recall_without_gb34"].is_null());
        assert_eq!(json["other_codes"]["E0308"]["extra"], 2);
    }

    #[test]
    fn fp_only_case_is_a_recall_hit() {
        let want = d("E0599", "a/src/lib.rs", 1);
        let extra = d("E0308", "a/src/lib.rs", 2);
        let report = Report {
            outcomes: vec![outcome(
                "a",
                Group::NoMethod,
                vec![want],
                fail(vec![], vec![extra]),
            )],
            excluded: vec![],
        };
        let c = report.counts()[&Group::NoMethod];
        assert_eq!((c.hit, c.of, c.exact, c.fp, c.fp_diags), (1, 1, 0, 1, 0));
        assert_eq!(report.primary_recall(), Some((1, 1)));
        assert_eq!(report.other_codes()["E0308"].extra, 1);
    }

    #[test]
    fn group_recall_counts_only_group_codes() {
        let own = d("E0599", "a/src/lib.rs", 1);
        let other = d("E0277", "a/src/lib.rs", 5);
        let report = Report {
            outcomes: vec![outcome(
                "a",
                Group::NoMethod,
                vec![own, other.clone()],
                fail(vec![other], vec![]),
            )],
            excluded: vec![],
        };
        let c = report.counts()[&Group::NoMethod];
        assert_eq!((c.hit, c.of), (1, 1));
        assert_eq!(c.tp_diags, 1);
        assert_eq!(
            report.other_codes()["E0277"],
            OtherCounts {
                missed: 1,
                extra: 0,
                cases: 1
            }
        );
    }

    #[test]
    fn valid_case_uses_exact_and_other_codes() {
        let extra = d("E0308", "a/src/lib.rs", 2);
        let report = Report {
            outcomes: vec![outcome(
                "v",
                Group::Valid,
                vec![],
                fail(vec![], vec![extra]),
            )],
            excluded: vec![],
        };
        let c = report.counts()[&Group::Valid];
        assert_eq!((c.of, c.exact, c.fp), (0, 0, 1));
        assert_eq!(report.other_codes()["E0308"].extra, 1);
        assert!(report.misgrouped().is_empty());
    }

    #[test]
    fn primary_recall_without_drops_tagged() {
        let want = d("E0425", "a/src/lib.rs", 3);
        let report = Report {
            outcomes: vec![outcome(
                "ws/a",
                Group::UnresolvedName,
                vec![want.clone()],
                fail(vec![want.clone()], vec![]),
            )],
            excluded: vec![],
        };
        let tags = vec![GapTag {
            row: "G-B-4".into(),
            case: "ws/a".into(),
            diagnostic: want,
        }];
        assert_eq!(report.primary_recall(), Some((0, 1)));
        assert_eq!(
            report.primary_recall_without(&tags, &["G-B-3", "G-B-4"]),
            None
        );
        // A row that is not asked for leaves the case in.
        assert_eq!(
            report.primary_recall_without(&tags, &["G-B-3"]),
            Some((0, 1))
        );
    }

    #[test]
    fn misgrouped_lists_cases_without_a_group_code() {
        let report = Report {
            outcomes: vec![outcome(
                "m",
                Group::NoMethod,
                vec![d("E0308", "a/src/lib.rs", 1)],
                Outcome::Pass,
            )],
            excluded: vec![],
        };
        assert_eq!(report.misgrouped().len(), 1);
        assert_eq!(report.counts()[&Group::NoMethod].of, 0);
    }

    #[test]
    fn cause_groups() {
        let causes: Vec<Group> = Group::ALL.into_iter().filter(|g| g.is_cause()).collect();
        assert_eq!(
            causes,
            [
                Group::Cfg,
                Group::CfgTest,
                Group::MacroDerive,
                Group::MacroAttr,
                Group::MacroRules,
                Group::MacroTokens,
                Group::ProcMacroEdit
            ]
        );
        assert_eq!(
            crate::breakage::group_for(
                crate::breakage::Kind::RemoveImport,
                &[d("E0599", "a/src/lib.rs", 1)]
            ),
            Group::NoMethod
        );
    }
}
