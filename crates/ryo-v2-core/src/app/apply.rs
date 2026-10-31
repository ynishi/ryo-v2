//! The use case: take a set of Blueprints, verify each against the analyzer, and say which
//! may be applied and how.
//!
//! This is a dry run. Nothing is written to disk: a passing Blueprint becomes a
//! [`Candidate`] that carries the whole-file [`Edit`]s it would make.

use crate::app::ports::{CheckError, Verifier};
use crate::domain::file_role::file_role;
use crate::domain::resolution::{DEFAULT_BUDGET, NEAR_MISS_MAX, risk_marks};
use crate::domain::text::splice;
use crate::domain::{
    Basis, Blocking, Blueprint, BlueprintId, BlueprintSet, Candidate, DecisionLog, Drift, Edit,
    Evidence, FileRole, Infeasible, LogEntry, NearMiss, NonEmpty, RelPath, Resolution, RuleCatalog,
    Scope, Stage, Verdict,
};
use std::collections::{BTreeMap, BTreeSet};

/// Why `apply_blueprints` could not finish.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// The analyzer check failed as a process, not as a verdict.
    #[error("check of blueprint {blueprint} failed: {source}")]
    Check {
        /// The Blueprint being checked.
        blueprint: BlueprintId,
        /// The failure.
        #[source]
        source: CheckError,
    },
}

enum Outcome {
    Candidate(Box<Candidate>),
    Blocked {
        blocking: Blocking,
        near_miss: Option<NearMiss>,
    },
}

/// What the stages leave behind while a set (or a search) is decided.
#[derive(Default)]
pub(crate) struct Acc {
    pub(crate) log: DecisionLog,
    pub(crate) verdicts: Vec<(BlueprintId, Verdict)>,
    pub(crate) fingerprints_checked: usize,
}

impl Acc {
    pub(crate) fn note(
        &mut self,
        blueprint: BlueprintId,
        stage: Stage,
        ok: bool,
        note: impl Into<String>,
    ) {
        self.log.push(LogEntry::new(blueprint, stage, ok, note));
    }
}

pub(crate) use crate::domain::basis::View;

/// A Blueprint that passed the stages before the analyzer check.
pub(crate) struct Ready {
    /// Whole-file edits, spliced against the view.
    pub(crate) edits: Vec<Edit>,
    /// The roles of the files it touches.
    pub(crate) roles: BTreeSet<FileRole>,
}

/// Outcome of the stages before the analyzer check.
pub(crate) enum Prepared {
    /// All stages passed.
    Ready(Ready),
    /// A stage refused it.
    Blocked {
        /// The stage that refused.
        stage: Stage,
        /// Why.
        blocking: Blocking,
    },
}

fn blocked(stage: Stage, blocking: Blocking) -> Prepared {
    Prepared::Blocked { stage, blocking }
}

/// Verifies every Blueprint of `set` against the analyzer and sorts out which may be applied.
///
/// # Contract
/// - C1: the freshness stage is `Basis::check` of `Blueprint::basis` (the check a candidate's
///   basis is checked with): a Blueprint with a base fingerprint that is not fresh, or a read
///   of a file without a base fingerprint that is not fresh, is blocked by one
///   `Blocking::Stale` that lists every such drift, and `Verifier::check` is not called for it.
///   tested: app::apply::tests::stale_blueprint_is_failed_and_never_checked
///   tested: app::apply::tests::identical_line_inserted_above_is_stale
///   tested: app::apply::tests::read_of_file_without_base_is_checked
///   tested: app::apply::tests::stale_lists_every_base_and_read
///   tested: app::apply::tests::read_of_file_with_base_but_no_spec_is_covered_by_its_base
///   tested: app::apply::tests::candidate_basis_is_stale_after_outside_change
/// - C2: a spec in a file whose role is `Manifest` is blocked as `Unverifiable` before
///   anything is read, and the check is not called; a build script or proc-macro file gives
///   a candidate whose `outer_check` is `Some`, which is never `AutoApplicable`.
///   tested: app::apply::tests::crate_graph_edits_never_pass_silently
/// - C3: a base fingerprint or a read of a file the analyzer does not have gives
///   `MissingFile`, and the check is not called.
///   tested: app::apply::tests::missing_file_is_blocking
/// - C4: an `Err` of the check returns `ApplyError::Check` at once.
///   tested: app::apply::tests::check_error_is_apply_error
/// - C5: the result is `Decided` when at least one Blueprint passed, with the candidates in
///   input order; a failed Blueprint with one to `NEAR_MISS_MAX` new errors is also listed
///   as a near miss. With no candidate the result is `Failed`.
///   tested: app::apply::tests::one_passing_of_two_is_decided_with_near_miss
/// - C6: each stage logs one entry per Blueprint, `ok` false at the stage that blocked it and
///   no entry after that.
///   tested: app::apply::tests::decision_log_stops_at_blocking_stage
/// - C7: the marks of an intent with `Borrow` and `Behaviour` risks give an outer check and
///   `NeedsReview{Behaviour}`, with or without a Rule for another intent.
///   tested: app::apply::tests::borrow_intent_reports_needs_outer_check
/// - C8: a passing Blueprint of rename-local or add-derive is `NeedsReview{NoRule}` under a
///   catalog that holds every Rule that can be registered.
///   tested: app::apply::tests::text_match_intents_need_review_under_every_rule
/// - C9: the marks and the Rule of a candidate are derived from its Blueprint's intent and the
///   roles of the files it edits; a Rule registered for another intent never covers it.
///   tested: app::apply::tests::rule_of_another_intent_never_covers
/// - C10: a Blueprint with a spec that writes the bytes it replaces is blocked at the
///   conformance stage as `Nonconforming` with `Nonconformance::NoOp`, and the check is not
///   called.
///   tested: app::apply::tests::no_op_rename_is_refused_and_never_checked
/// - C11: a `Decided` resolution's evidence carries the verifier's provenance unchanged.
///   tested: app::apply::tests::evidence_carries_the_provenance
pub fn apply_blueprints<V: Verifier>(
    v: &V,
    s: &mut V::Session,
    set: &BlueprintSet,
    rules: &RuleCatalog,
    scope: Scope,
) -> Result<Resolution, ApplyError> {
    let baseline = v.baseline(s);
    let provenance = v.provenance(s);
    let mut acc = Acc::default();
    let mut candidates = Vec::new();
    let mut blocking = Vec::new();
    let mut near_miss = Vec::new();
    for bp in set.iter() {
        match decide(v, s, bp, &baseline, rules, scope, &mut acc)? {
            Outcome::Candidate(c) => candidates.push(*c),
            Outcome::Blocked {
                blocking: b,
                near_miss: n,
            } => {
                blocking.push(b);
                near_miss.extend(n);
            }
        }
    }
    if let Some(blueprints) = NonEmpty::new(candidates) {
        return Ok(Resolution::Decided {
            blueprints,
            decision_log: acc.log,
            evidence: Evidence::new(
                scope,
                acc.fingerprints_checked,
                baseline.len(),
                acc.verdicts,
                DEFAULT_BUDGET,
                provenance,
            ),
        });
    }
    // A set has at least one Blueprint and each one ends as a candidate or a blocking, so
    // `blocking` is not empty here; the fallback keeps the type honest.
    let infeasible =
        Infeasible::new(blocking, near_miss, false).unwrap_or_else(Infeasible::exhausted);
    Ok(Resolution::Failed(infeasible))
}

/// The stages before the analyzer check, against the texts of `view`: file role, freshness of
/// the base fingerprints and of the reads of files without one, conformance, splice and marks.
/// Each stage logs one entry to `acc`, `ok` false at the stage that refuses.
pub(crate) fn prepare<V: Verifier>(
    v: &V,
    s: &V::Session,
    bp: &Blueprint,
    view: &View<'_>,
    acc: &mut Acc,
) -> Prepared {
    let id = bp.id();
    let by_file = bp.edits_by_file();

    // 1. file roles
    let mut roles = BTreeSet::new();
    for path in by_file.keys() {
        let role = file_role(path, &v.facts(s, path));
        if role == FileRole::Manifest {
            acc.note(id, Stage::FileRole, false, format!("{path} is a {role}"));
            return blocked(
                Stage::FileRole,
                Blocking::Unverifiable {
                    blueprint: id,
                    path: (*path).clone(),
                    role,
                },
            );
        }
        roles.insert(role);
    }
    acc.note(
        id,
        Stage::FileRole,
        true,
        format!("{} file(s)", by_file.len()),
    );

    // 2. freshness
    let basis = bp.basis();
    let drift = basis.check(view);
    let missing = drift
        .iter()
        .filter(|d| matches!(d, Drift::Missing(_)))
        .count();
    acc.fingerprints_checked += basis.files().len() + basis.reads().len() - missing;
    if let Some(Drift::Missing(path)) = drift.iter().find(|d| matches!(d, Drift::Missing(_))) {
        acc.note(
            id,
            Stage::Freshness,
            false,
            format!("{path} is not in the workspace"),
        );
        return blocked(
            Stage::Freshness,
            Blocking::MissingFile {
                blueprint: id,
                path: path.clone(),
            },
        );
    }
    if !drift.is_empty() {
        let files = drift
            .iter()
            .filter(|d| matches!(d, Drift::File { .. }))
            .count();
        acc.note(
            id,
            Stage::Freshness,
            false,
            format!(
                "{files} base(s) and {} read(s) are not fresh",
                drift.len() - files
            ),
        );
        return blocked(
            Stage::Freshness,
            Blocking::Stale {
                blueprint: id,
                drift,
            },
        );
    }
    acc.note(
        id,
        Stage::Freshness,
        true,
        format!(
            "{} base(s) and {} read(s) fresh",
            basis.files().len(),
            basis.reads().len()
        ),
    );

    // 3. conformance
    let base: BTreeMap<RelPath, String> = by_file
        .keys()
        .filter_map(|p| view(p).map(|t| ((*p).clone(), t)))
        .collect();
    if let Err(why) = bp.intent().conforms(bp.specs(), &base) {
        acc.note(id, Stage::Conformance, false, why.to_string());
        return blocked(
            Stage::Conformance,
            Blocking::Nonconforming { blueprint: id, why },
        );
    }
    acc.note(id, Stage::Conformance, true, bp.intent().to_string());

    // 4. splice
    let mut edits = Vec::new();
    for (path, specs) in &by_file {
        let Some(text) = base.get(*path) else {
            // Cannot happen after the freshness stage; reported as a missing file all the same.
            acc.note(id, Stage::Splice, false, format!("{path} has no text"));
            return blocked(
                Stage::Splice,
                Blocking::MissingFile {
                    blueprint: id,
                    path: (*path).clone(),
                },
            );
        };
        match splice(text, specs) {
            Ok(new_text) => edits.push(Edit::write((*path).clone(), new_text)),
            Err(err) => {
                acc.note(id, Stage::Splice, false, err.to_string());
                return blocked(Stage::Splice, Blocking::Splice { blueprint: id, err });
            }
        }
    }
    acc.note(id, Stage::Splice, true, format!("{} edit(s)", edits.len()));

    // 5. marks
    let marks = risk_marks([bp.intent().kind()], roles.iter().copied());
    let names: Vec<String> = marks.iter().map(ToString::to_string).collect();
    acc.note(id, Stage::Marks, true, names.join(", "));

    Prepared::Ready(Ready { edits, roles })
}

fn decide<V: Verifier>(
    v: &V,
    s: &mut V::Session,
    bp: &Blueprint,
    baseline: &[crate::domain::Finding],
    rules: &RuleCatalog,
    scope: Scope,
    acc: &mut Acc,
) -> Result<Outcome, ApplyError> {
    let id = bp.id();
    let prepared = {
        let session: &V::Session = s;
        prepare(v, session, bp, &|p| v.text(session, p), acc)
    };
    let Ready { edits, roles } = match prepared {
        Prepared::Ready(r) => r,
        Prepared::Blocked { blocking, .. } => {
            return Ok(Outcome::Blocked {
                blocking,
                near_miss: None,
            });
        }
    };

    // 6. check
    let found = v
        .check(s, &edits, scope)
        .map_err(|source| ApplyError::Check {
            blueprint: id,
            source,
        })?;
    let verdict = Verdict::from_baseline(baseline, &found);
    let basis = {
        let session: &V::Session = s;
        Basis::of_chain(&[bp], &edits, &|p| v.text(session, p))
    };
    acc.verdicts.push((id, verdict.clone()));
    let errors = verdict.new_errors.errors.len() + verdict.new_errors.unmapped.len();
    acc.note(
        id,
        Stage::Check,
        verdict.passed(),
        format!("{errors} new error(s)"),
    );

    // 7. candidate
    match Candidate::new(
        NonEmpty::one(bp.clone()),
        &roles,
        edits,
        verdict,
        basis,
        rules,
        scope,
    ) {
        Ok(c) => {
            acc.note(id, Stage::Candidate, true, c.disposition().to_string());
            Ok(Outcome::Candidate(Box::new(c)))
        }
        Err(verdict) => {
            let near_miss = (1..=NEAR_MISS_MAX).contains(&errors).then_some(NearMiss {
                blueprint: id,
                new_errors: errors,
            });
            Ok(Outcome::Blocked {
                blocking: Blocking::NewErrors {
                    blueprint: id,
                    verdict,
                },
                near_miss,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ports::FileFacts;
    use crate::domain::{
        BaseFp, BlueprintError, BlueprintParts, DecisionRecord, DeriveName, Disposition, ErrorCode,
        Finding, FindingCode, Freshness, Ident, IntentCall, IntentKind, MutationSpec, NonEmpty,
        Nonconformance, OuterCheckReason, Provenance, ReadRange, ReviewReason, ReviewRecord,
        RiskMark, Rule, RuleId, ScopedFindings, SpecRef, TextRange,
    };
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    #[derive(Default)]
    struct Fake {
        texts: BTreeMap<RelPath, String>,
        facts: BTreeMap<RelPath, FileFacts>,
        baseline: Vec<Finding>,
        provenance: Option<Provenance>,
        results: RefCell<VecDeque<Result<ScopedFindings, CheckError>>>,
        checks: Cell<usize>,
    }

    impl Verifier for Fake {
        type Session = ();
        fn text(&self, _: &(), path: &RelPath) -> Option<String> {
            self.texts.get(path).cloned()
        }
        fn facts(&self, _: &(), path: &RelPath) -> FileFacts {
            self.facts.get(path).copied().unwrap_or_default()
        }
        fn baseline(&self, _: &()) -> Vec<Finding> {
            self.baseline.clone()
        }
        fn provenance(&self, _: &()) -> Provenance {
            self.provenance.clone().unwrap_or_else(prov)
        }
        fn check(&self, _: &mut (), _: &[Edit], _: Scope) -> Result<ScopedFindings, CheckError> {
            self.checks.set(self.checks.get() + 1);
            self.results
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Ok(ScopedFindings::default()))
        }
    }

    impl Fake {
        fn with_text(path: &str, text: &str) -> Self {
            let mut f = Self::default();
            f.texts.insert(rel(path), text.to_owned());
            f
        }
        fn queue(&self, r: Result<ScopedFindings, CheckError>) {
            self.results.borrow_mut().push_back(r);
        }
    }

    const BASE: &str = "let a = 1;";

    fn prov() -> Provenance {
        Provenance::new(
            "fake",
            "0",
            BTreeMap::new(),
            BTreeMap::from([(rel("src/lib.rs"), crate::domain::Fingerprint::of(b"x"))]),
        )
        .unwrap()
    }

    fn rel(s: &str) -> RelPath {
        RelPath::parse(s).unwrap()
    }
    fn bid(n: u8) -> BlueprintId {
        BlueprintId::parse(&format!("01890a5d-ac96-774b-bcce-b302099a80{n:02x}")).unwrap()
    }
    fn tr(a: u32, b: u32) -> TextRange {
        TextRange::new(a, b).unwrap()
    }
    fn make(
        n: u8,
        intent: IntentCall,
        specs: Vec<MutationSpec>,
        path: &str,
        text: &str,
    ) -> Result<Blueprint, BlueprintError> {
        let end = u32::try_from(text.len()).unwrap();
        Blueprint::new(BlueprintParts {
            id: bid(n),
            intent,
            specs,
            reads: vec![ReadRange::capture(rel(path), text, tr(0, end)).unwrap()],
            bases: vec![BaseFp::capture(rel(path), text)],
            spec_refs: vec![SpecRef::parse("spec#t").unwrap()],
            record: DecisionRecord::new("agent", "why").unwrap(),
        })
    }
    /// Renames `a` to `b` in `BASE` at `path`.
    fn rename(n: u8, path: &str) -> Blueprint {
        make(
            n,
            IntentCall::RenameLocal {
                from: Ident::parse("a").unwrap(),
                to: Ident::parse("b").unwrap(),
            },
            vec![MutationSpec::replace(rel(path), tr(4, 5), "b").unwrap()],
            path,
            BASE,
        )
        .unwrap()
    }
    fn extract(n: u8, path: &str) -> Blueprint {
        let text = "f(1 + 2);";
        make(
            n,
            IntentCall::ExtractVariable {
                name: Ident::parse("t").unwrap(),
            },
            vec![
                MutationSpec::insert(rel(path), 0, "let t = 1 + 2;\n").unwrap(),
                MutationSpec::replace(rel(path), tr(2, 7), "t").unwrap(),
            ],
            path,
            text,
        )
        .unwrap()
    }
    fn set(bps: Vec<Blueprint>) -> BlueprintSet {
        BlueprintSet::new(bps).unwrap()
    }
    fn errors(n: usize) -> ScopedFindings {
        ScopedFindings {
            files: vec![rel("src/lib.rs")],
            findings: (0..n)
                .map(|i| Finding {
                    code: FindingCode::Rustc(ErrorCode::parse("E0599").unwrap()),
                    message: format!("m{i}"),
                    path: rel("src/lib.rs"),
                    line: 1,
                })
                .collect(),
        }
    }
    /// One Rule for each kind of `IntentKind::ALL` that `Rule::register` accepts.
    fn every_rule() -> RuleCatalog {
        let rules = IntentKind::ALL
            .iter()
            .zip(0u8..)
            .filter_map(|(k, n)| {
                Rule::register(
                    RuleId::parse(&format!(
                        "01890a5d-ac96-774b-bcce-b302099a80{:02x}",
                        0x90 + n
                    ))
                    .unwrap(),
                    *k,
                    ReviewRecord {
                        reviewer: "me".to_owned(),
                        reviewed_at_ms: 1,
                        note: String::new(),
                    },
                    Scope::Affected,
                )
                .ok()
            })
            .collect();
        RuleCatalog::new(rules).unwrap()
    }
    fn add_derive(n: u8, path: &str) -> Blueprint {
        let text = "struct S;";
        make(
            n,
            IntentCall::AddDerive {
                derives: NonEmpty::new(vec![DeriveName::parse("Clone").unwrap()]).unwrap(),
            },
            vec![MutationSpec::insert(rel(path), 0, "#[derive(Clone)]\n").unwrap()],
            path,
            text,
        )
        .unwrap()
    }
    fn run(
        fake: &Fake,
        bps: Vec<Blueprint>,
        rules: &RuleCatalog,
    ) -> Result<Resolution, ApplyError> {
        apply_blueprints(fake, &mut (), &set(bps), rules, Scope::Workspace)
    }
    fn failed(r: Result<Resolution, ApplyError>) -> Infeasible {
        match r.unwrap() {
            Resolution::Failed(i) => i,
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn no_op_rename_is_refused_and_never_checked() {
        let fake = Fake::with_text("src/lib.rs", BASE);
        let same = make(
            1,
            IntentCall::RenameLocal {
                from: Ident::parse("a").unwrap(),
                to: Ident::parse("a").unwrap(),
            },
            vec![MutationSpec::replace(rel("src/lib.rs"), tr(4, 5), "a").unwrap()],
            "src/lib.rs",
            BASE,
        )
        .unwrap();
        let i = failed(run(&fake, vec![same], &every_rule()));
        assert_eq!(fake.checks.get(), 0);
        assert!(
            matches!(
                &i.blocking()[0],
                Blocking::Nonconforming { blueprint, why: Nonconformance::NoOp { spec: 0 } }
                    if *blueprint == bid(1)
            ),
            "{:?}",
            i.blocking()
        );
    }

    #[test]
    fn stale_blueprint_is_failed_and_never_checked() {
        let fake = Fake::with_text("src/lib.rs", "let a = 2;");
        let i = failed(run(
            &fake,
            vec![rename(1, "src/lib.rs")],
            &RuleCatalog::default(),
        ));
        assert_eq!(fake.checks.get(), 0);
        assert_eq!(i.blocking().len(), 1);
        match &i.blocking()[0] {
            Blocking::Stale { blueprint, drift } => {
                assert_eq!(*blueprint, bid(1));
                // the read is of a file with a base, so the base covers it
                assert_eq!(drift.len(), 1);
                assert!(matches!(
                    &drift[0],
                    Drift::File { base, now: Freshness::Stale { .. } }
                        if base.path() == &rel("src/lib.rs")
                ));
            }
            other => panic!("{other:?}"),
        }
        assert!(!i.budget_exhausted());
    }

    #[test]
    fn identical_line_inserted_above_is_stale() {
        let base = "fn f() {\n    let a = 1;\n}\n";
        let path = rel("src/lib.rs");
        // line 2 is bytes 9..24; the spec renames the `a` at 17..18
        let bp = Blueprint::new(BlueprintParts {
            id: bid(1),
            intent: IntentCall::RenameLocal {
                from: Ident::parse("a").unwrap(),
                to: Ident::parse("b").unwrap(),
            },
            specs: vec![MutationSpec::replace(path.clone(), tr(17, 18), "b").unwrap()],
            reads: vec![ReadRange::capture(path.clone(), base, tr(9, 24)).unwrap()],
            bases: vec![BaseFp::capture(path.clone(), base)],
            spec_refs: vec![],
            record: DecisionRecord::new("agent", "why").unwrap(),
        })
        .unwrap();
        // The bytes of the read are unchanged, but the file is not the base any more.
        let moved = "fn f() {\n    let a = 1;\n    let a = 1;\n}\n";
        assert_eq!(bp.reads()[0].check(moved), Freshness::Fresh);
        let fake = Fake::with_text("src/lib.rs", moved);
        let i = failed(run(&fake, vec![bp], &RuleCatalog::default()));
        assert_eq!(fake.checks.get(), 0);
        assert_eq!(i.blocking().len(), 1);
        match &i.blocking()[0] {
            Blocking::Stale { drift, .. } => {
                assert_eq!(drift.len(), 1);
                assert!(matches!(
                    &drift[0],
                    Drift::File { base, now: Freshness::Stale { .. } } if base.path() == &path
                ));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn read_of_file_without_base_is_checked() {
        let path = rel("src/lib.rs");
        let other = rel("src/o.rs");
        let bp = Blueprint::new(BlueprintParts {
            id: bid(1),
            intent: IntentCall::RenameLocal {
                from: Ident::parse("a").unwrap(),
                to: Ident::parse("b").unwrap(),
            },
            specs: vec![MutationSpec::replace(path.clone(), tr(4, 5), "b").unwrap()],
            reads: vec![
                ReadRange::capture(path.clone(), BASE, tr(0, 10)).unwrap(),
                ReadRange::capture(other.clone(), "abc", tr(0, 3)).unwrap(),
            ],
            bases: vec![BaseFp::capture(path, BASE)],
            spec_refs: vec![],
            record: DecisionRecord::new("agent", "why").unwrap(),
        })
        .unwrap();
        let mut fake = Fake::with_text("src/lib.rs", BASE);
        // the file that is only read is missing
        let i = failed(run(&fake, vec![bp.clone()], &RuleCatalog::default()));
        assert_eq!(
            i.blocking(),
            &[Blocking::MissingFile {
                blueprint: bid(1),
                path: other.clone()
            }]
        );
        // it changed
        fake.texts.insert(other.clone(), "abd".to_owned());
        let i = failed(run(&fake, vec![bp.clone()], &RuleCatalog::default()));
        assert_eq!(fake.checks.get(), 0);
        match &i.blocking()[0] {
            Blocking::Stale { drift, .. } => {
                assert_eq!(drift.len(), 1);
                assert!(matches!(
                    &drift[0],
                    Drift::Read { read, now: Freshness::Stale { .. } } if read.path() == &other
                ));
            }
            other => panic!("{other:?}"),
        }
        // it is fresh: one base and one read were compared
        fake.texts.insert(other, "abc".to_owned());
        let r = run(&fake, vec![bp], &RuleCatalog::default()).unwrap();
        let Resolution::Decided { evidence, .. } = r else {
            panic!("expected Decided");
        };
        assert_eq!(evidence.fingerprints_checked(), 2);
    }

    #[test]
    fn stale_lists_every_base_and_read() {
        let (a, b, c) = (rel("src/a.rs"), rel("src/b.rs"), rel("src/c.rs"));
        let bp = |n: u8| {
            Blueprint::new(BlueprintParts {
                id: bid(n),
                intent: IntentCall::RenameLocal {
                    from: Ident::parse("a").unwrap(),
                    to: Ident::parse("b").unwrap(),
                },
                specs: vec![
                    MutationSpec::replace(a.clone(), tr(4, 5), "b").unwrap(),
                    MutationSpec::replace(b.clone(), tr(4, 5), "b").unwrap(),
                ],
                reads: vec![
                    ReadRange::capture(a.clone(), BASE, tr(0, 10)).unwrap(),
                    ReadRange::capture(b.clone(), BASE, tr(0, 10)).unwrap(),
                    ReadRange::capture(c.clone(), "abc", tr(0, 3)).unwrap(),
                ],
                bases: vec![
                    BaseFp::capture(b.clone(), BASE),
                    BaseFp::capture(a.clone(), BASE),
                ],
                spec_refs: vec![],
                record: DecisionRecord::new("agent", "why").unwrap(),
            })
            .unwrap()
        };
        let mut fake = Fake::with_text("src/a.rs", "x let a = 1;");
        fake.texts.insert(b.clone(), "let a = 1; y".to_owned());
        fake.texts.insert(c.clone(), "abd".to_owned());
        let i = failed(run(&fake, vec![bp(1)], &RuleCatalog::default()));
        assert_eq!(fake.checks.get(), 0);
        assert_eq!(i.blocking().len(), 1);
        match &i.blocking()[0] {
            Blocking::Stale { drift, .. } => {
                let kinds: Vec<(&str, &RelPath)> = drift
                    .iter()
                    .map(|d| match d {
                        Drift::File { .. } => ("file", d.path()),
                        Drift::Read { .. } => ("read", d.path()),
                        Drift::Missing(_) => ("missing", d.path()),
                    })
                    .collect();
                assert_eq!(kinds, vec![("file", &a), ("file", &b), ("read", &c)]);
            }
            other => panic!("{other:?}"),
        }
        assert!(
            i.blocking()[0]
                .to_string()
                .ends_with("2 base(s) and 1 read(s) are stale"),
            "{}",
            i.blocking()[0]
        );
        // the same Blueprint next to one that passes: the log keeps the counts
        fake.texts.insert(rel("src/ok.rs"), BASE.to_owned());
        let r = run(
            &fake,
            vec![bp(1), rename(2, "src/ok.rs")],
            &RuleCatalog::default(),
        )
        .unwrap();
        let Resolution::Decided { decision_log, .. } = r else {
            panic!("expected Decided");
        };
        let fresh: Vec<_> = decision_log
            .entries()
            .iter()
            .filter(|e| e.blueprint() == bid(1) && e.stage() == Stage::Freshness)
            .collect();
        assert_eq!(fresh.len(), 1);
        assert!(!fresh[0].ok());
        assert_eq!(fresh[0].note(), "2 base(s) and 1 read(s) are not fresh");
    }

    #[test]
    fn candidate_basis_is_stale_after_outside_change() {
        let lib = rel("src/lib.rs");
        let fake = Fake::with_text("src/lib.rs", BASE);
        let r = run(
            &fake,
            vec![rename(1, "src/lib.rs")],
            &RuleCatalog::default(),
        )
        .unwrap();
        let Resolution::Decided { blueprints, .. } = r else {
            panic!("expected Decided");
        };
        let basis = blueprints.first().basis();
        assert_eq!(basis.files()[0], BaseFp::capture(lib.clone(), BASE));
        let original = |p: &RelPath| (p == &lib).then(|| BASE.to_owned());
        assert!(basis.check(&original).is_empty());
        let changed = |p: &RelPath| (p == &lib).then(|| format!("{BASE} //"));
        let drift = basis.check(&changed);
        assert_eq!(drift.len(), 1);
        assert!(matches!(
            &drift[0],
            Drift::File { base, now: Freshness::Stale { .. } } if base.path() == &lib
        ));
    }

    #[test]
    fn second_alternative_is_stale_after_first_is_written() {
        let lib = rel("src/lib.rs");
        let second = make(
            2,
            IntentCall::RenameLocal {
                from: Ident::parse("a").unwrap(),
                to: Ident::parse("c").unwrap(),
            },
            vec![MutationSpec::replace(lib.clone(), tr(4, 5), "c").unwrap()],
            "src/lib.rs",
            BASE,
        )
        .unwrap();
        let fake = Fake::with_text("src/lib.rs", BASE);
        let r = run(
            &fake,
            vec![rename(1, "src/lib.rs"), second],
            &RuleCatalog::default(),
        )
        .unwrap();
        let Resolution::Decided { blueprints, .. } = r else {
            panic!("expected Decided");
        };
        assert_eq!(blueprints.len(), 2);
        let (c1, c2) = (&blueprints.as_slice()[0], &blueprints.as_slice()[1]);
        let written = c1.edits()[0].new_text.clone().unwrap();
        let after = |p: &RelPath| (p == &lib).then(|| written.clone());
        let drift = c2.basis().check(&after);
        assert_eq!(drift.len(), 1);
        assert!(matches!(&drift[0], Drift::File { .. }));
        let original = |p: &RelPath| (p == &lib).then(|| BASE.to_owned());
        assert!(c1.basis().check(&original).is_empty());
    }

    #[test]
    fn read_of_file_with_base_but_no_spec_is_covered_by_its_base() {
        let (a, c) = (rel("src/a.rs"), rel("src/c.rs"));
        let bp = Blueprint::new(BlueprintParts {
            id: bid(1),
            intent: IntentCall::RenameLocal {
                from: Ident::parse("a").unwrap(),
                to: Ident::parse("b").unwrap(),
            },
            specs: vec![MutationSpec::replace(a.clone(), tr(4, 5), "b").unwrap()],
            reads: vec![
                ReadRange::capture(a.clone(), BASE, tr(0, 10)).unwrap(),
                // wrong on purpose: were it checked, it would be stale
                ReadRange::new(
                    c.clone(),
                    tr(0, 10),
                    crate::domain::Fingerprint::of(b"other"),
                ),
            ],
            bases: vec![BaseFp::capture(a, BASE), BaseFp::capture(c, BASE)],
            spec_refs: vec![],
            record: DecisionRecord::new("agent", "why").unwrap(),
        })
        .unwrap();
        let mut fake = Fake::with_text("src/a.rs", BASE);
        fake.texts.insert(rel("src/c.rs"), BASE.to_owned());
        let r = run(&fake, vec![bp], &RuleCatalog::default()).unwrap();
        let Resolution::Decided { evidence, .. } = r else {
            panic!("expected Decided");
        };
        assert_eq!(fake.checks.get(), 1);
        assert_eq!(evidence.fingerprints_checked(), 2);
    }

    #[test]
    fn one_passing_of_two_is_decided_with_near_miss() {
        let mut fake = Fake::with_text("src/lib.rs", BASE);
        fake.baseline = vec![];
        fake.queue(Ok(ScopedFindings::default()));
        fake.queue(Ok(errors(1)));
        fake.queue(Ok(errors(NEAR_MISS_MAX + 1)));
        let r = run(
            &fake,
            vec![
                rename(1, "src/lib.rs"),
                rename(2, "src/lib.rs"),
                rename(3, "src/lib.rs"),
            ],
            &every_rule(),
        )
        .unwrap();
        assert_eq!(fake.checks.get(), 3);
        let Resolution::Decided {
            blueprints,
            decision_log,
            evidence,
        } = r
        else {
            panic!("expected Decided");
        };
        assert_eq!(blueprints.len(), 1);
        let c = blueprints.first();
        assert_eq!(c.blueprint(), bid(1));
        assert_eq!(c.edits(), &[Edit::write(rel("src/lib.rs"), "let b = 1;")]);
        assert_eq!(
            c.disposition(),
            Disposition::NeedsReview {
                reason: ReviewReason::NoRule
            }
        );
        assert_eq!(evidence.verdicts().len(), 3);
        assert_eq!(evidence.fingerprints_checked(), 3);
        assert_eq!(evidence.baseline_size(), 0);
        assert_eq!(evidence.scope(), Scope::Workspace);
        assert_eq!(evidence.budget(), DEFAULT_BUDGET);
        let last_of = |n| {
            decision_log
                .entries()
                .iter()
                .rfind(|e| e.blueprint() == bid(n))
                .unwrap()
                .clone()
        };
        assert_eq!(
            (last_of(1).stage(), last_of(1).ok()),
            (Stage::Candidate, true)
        );
        assert_eq!((last_of(2).stage(), last_of(2).ok()), (Stage::Check, false));

        // The near-miss rule, seen through a run with no candidate.
        let fake = Fake::with_text("src/lib.rs", BASE);
        fake.queue(Ok(errors(1)));
        fake.queue(Ok(errors(NEAR_MISS_MAX + 1)));
        let i = failed(run(
            &fake,
            vec![rename(2, "src/lib.rs"), rename(3, "src/lib.rs")],
            &RuleCatalog::default(),
        ));
        assert_eq!(i.blocking().len(), 2);
        assert!(matches!(i.blocking()[0], Blocking::NewErrors { .. }));
        assert_eq!(
            i.near_miss(),
            &[NearMiss {
                blueprint: bid(2),
                new_errors: 1
            }]
        );
    }

    #[test]
    fn borrow_intent_reports_needs_outer_check() {
        let fake = Fake::with_text("src/lib.rs", "f(1 + 2);");
        for rules in [RuleCatalog::default(), every_rule()] {
            let r = run(&fake, vec![extract(1, "src/lib.rs")], &rules).unwrap();
            let Resolution::Decided { blueprints, .. } = r else {
                panic!("expected Decided");
            };
            let c = blueprints.first();
            assert_eq!(
                c.outer_check().unwrap().reasons(),
                &BTreeSet::from([OuterCheckReason::Borrow])
            );
            assert_eq!(
                c.disposition(),
                Disposition::NeedsReview {
                    reason: ReviewReason::Behaviour
                }
            );
            assert_eq!(c.edits().len(), 1);
        }
    }

    #[test]
    fn text_match_intents_need_review_under_every_rule() {
        let mut fake = Fake::with_text("src/lib.rs", BASE);
        fake.texts.insert(rel("src/d.rs"), "struct S;".to_owned());
        let r = run(
            &fake,
            vec![rename(1, "src/lib.rs"), add_derive(2, "src/d.rs")],
            &every_rule(),
        )
        .unwrap();
        let Resolution::Decided { blueprints, .. } = r else {
            panic!("expected Decided");
        };
        assert_eq!(blueprints.len(), 2);
        for c in blueprints.as_slice() {
            assert_eq!(
                c.disposition(),
                Disposition::NeedsReview {
                    reason: ReviewReason::NoRule
                }
            );
        }
    }

    /// A move of `foo` from `src/a.rs` to `src/b.rs`, which conforms to its intent.
    fn move_foo(n: u8) -> Blueprint {
        let a = "pub fn foo() {}\n";
        Blueprint::new(BlueprintParts {
            id: bid(n),
            intent: IntentCall::MoveItem {
                item: Ident::parse("foo").unwrap(),
                to: rel("src/b.rs"),
            },
            specs: vec![
                MutationSpec::replace(rel("src/a.rs"), tr(0, 15), "pub use crate::b::foo;")
                    .unwrap(),
                MutationSpec::insert(rel("src/b.rs"), 0, a).unwrap(),
            ],
            reads: vec![
                ReadRange::capture(rel("src/a.rs"), a, tr(0, 16)).unwrap(),
                ReadRange::capture(rel("src/b.rs"), "", tr(0, 0)).unwrap(),
            ],
            bases: vec![
                BaseFp::capture(rel("src/a.rs"), a),
                BaseFp::capture(rel("src/b.rs"), ""),
            ],
            spec_refs: vec![SpecRef::parse("spec#t").unwrap()],
            record: DecisionRecord::new("agent", "why").unwrap(),
        })
        .unwrap()
    }

    #[test]
    fn rule_of_another_intent_never_covers() {
        let move_rule = Rule::register(
            RuleId::parse("01890a5d-ac96-774b-bcce-b302099a80a0").unwrap(),
            IntentKind::MoveItem,
            ReviewRecord {
                reviewer: "me".to_owned(),
                reviewed_at_ms: 1,
                note: String::new(),
            },
            Scope::Affected,
        )
        .unwrap();
        let rule_id = move_rule.id();
        let rules = RuleCatalog::new(vec![move_rule]).unwrap();
        let mut fake = Fake::with_text("src/lib.rs", BASE);
        fake.texts.insert(rel("src/x.rs"), "f(1 + 2);".to_owned());
        fake.texts
            .insert(rel("src/a.rs"), "pub fn foo() {}\n".to_owned());
        fake.texts.insert(rel("src/b.rs"), String::new());
        let r = run(
            &fake,
            vec![rename(1, "src/lib.rs"), extract(2, "src/x.rs"), move_foo(3)],
            &rules,
        )
        .unwrap();
        let Resolution::Decided { blueprints, .. } = r else {
            panic!("expected Decided");
        };
        let cs = blueprints.as_slice();
        assert_eq!(cs.len(), 3);
        assert_eq!(
            cs[0].disposition(),
            Disposition::NeedsReview {
                reason: ReviewReason::NoRule
            }
        );
        assert_eq!(
            cs[1].disposition(),
            Disposition::NeedsReview {
                reason: ReviewReason::Behaviour
            }
        );
        assert_eq!(
            cs[2].disposition(),
            Disposition::AfterOuterCheck { rule: rule_id }
        );
        assert_eq!(cs[2].marks(), &BTreeSet::from([RiskMark::Visibility]));
        assert_eq!(cs[2].path().len(), 1);
    }

    #[test]
    fn evidence_carries_the_provenance() {
        let mut fake = Fake::with_text("src/lib.rs", BASE);
        let given = Provenance::new(
            "other",
            "9.9",
            BTreeMap::from([("k".to_owned(), "v".to_owned())]),
            BTreeMap::new(),
        )
        .unwrap();
        fake.provenance = Some(given.clone());
        let r = run(
            &fake,
            vec![rename(1, "src/lib.rs")],
            &RuleCatalog::default(),
        )
        .unwrap();
        let Resolution::Decided { evidence, .. } = r else {
            panic!("expected Decided");
        };
        assert_eq!(evidence.provenance(), &given);
    }

    #[test]
    fn missing_file_is_blocking() {
        let fake = Fake::default();
        let i = failed(run(
            &fake,
            vec![rename(1, "src/lib.rs")],
            &RuleCatalog::default(),
        ));
        assert_eq!(fake.checks.get(), 0);
        assert_eq!(
            i.blocking(),
            &[Blocking::MissingFile {
                blueprint: bid(1),
                path: rel("src/lib.rs")
            }]
        );
    }

    #[test]
    fn check_error_is_apply_error() {
        let fake = Fake::with_text("src/lib.rs", BASE);
        fake.queue(Err(CheckError::Cancelled));
        let e = run(
            &fake,
            vec![rename(1, "src/lib.rs")],
            &RuleCatalog::default(),
        )
        .unwrap_err();
        match e {
            ApplyError::Check { blueprint, source } => {
                assert_eq!(blueprint, bid(1));
                assert_eq!(source, CheckError::Cancelled);
            }
        }
    }

    #[test]
    fn decision_log_stops_at_blocking_stage() {
        let mut fake = Fake::with_text("src/lib.rs", BASE);
        fake.texts
            .insert(rel("src/other.rs"), "let a = 2;".to_owned());
        let r = run(
            &fake,
            vec![rename(1, "src/other.rs"), rename(2, "src/lib.rs")],
            &RuleCatalog::default(),
        )
        .unwrap();
        let Resolution::Decided { decision_log, .. } = r else {
            panic!("expected Decided");
        };
        let stages = |n| -> Vec<(Stage, bool)> {
            decision_log
                .entries()
                .iter()
                .filter(|e| e.blueprint() == bid(n))
                .map(|e| (e.stage(), e.ok()))
                .collect()
        };
        assert_eq!(
            stages(1),
            vec![(Stage::FileRole, true), (Stage::Freshness, false)]
        );
        assert_eq!(
            stages(2),
            vec![
                (Stage::FileRole, true),
                (Stage::Freshness, true),
                (Stage::Conformance, true),
                (Stage::Splice, true),
                (Stage::Marks, true),
                (Stage::Check, true),
                (Stage::Candidate, true),
            ]
        );
        assert_eq!(fake.checks.get(), 1);
    }

    #[test]
    fn crate_graph_edits_never_pass_silently() {
        let rules = every_rule();
        // A manifest is not verifiable, whatever the facts say, and is never checked.
        let mut fake = Fake::with_text("Cargo.toml", BASE);
        fake.facts.insert(
            rel("Cargo.toml"),
            FileFacts {
                is_build_script: true,
                in_proc_macro_crate: true,
            },
        );
        let i = failed(run(&fake, vec![rename(1, "Cargo.toml")], &rules));
        assert_eq!(fake.checks.get(), 0);
        assert_eq!(
            i.blocking(),
            &[Blocking::Unverifiable {
                blueprint: bid(1),
                path: rel("Cargo.toml"),
                role: FileRole::Manifest
            }]
        );

        // A build script and a proc-macro file pass the check but never auto-apply.
        let mut fake = Fake::with_text("build.rs", BASE);
        fake.texts.insert(rel("m/src/lib.rs"), BASE.to_owned());
        fake.facts.insert(
            rel("build.rs"),
            FileFacts {
                is_build_script: true,
                in_proc_macro_crate: false,
            },
        );
        fake.facts.insert(
            rel("m/src/lib.rs"),
            FileFacts {
                is_build_script: false,
                in_proc_macro_crate: true,
            },
        );
        let r = run(
            &fake,
            vec![rename(1, "build.rs"), rename(2, "m/src/lib.rs")],
            &rules,
        )
        .unwrap();
        let Resolution::Decided { blueprints, .. } = r else {
            panic!("expected Decided");
        };
        let cs = blueprints.as_slice();
        assert_eq!(cs.len(), 2);
        assert!(cs[0].outer_check().is_some() && cs[1].outer_check().is_some());
        assert_eq!(
            cs[0].disposition(),
            Disposition::NeedsReview {
                reason: ReviewReason::NoRule
            }
        );
        assert_eq!(
            cs[1].disposition(),
            Disposition::NeedsReview {
                reason: ReviewReason::ProcMacroCrate
            }
        );
    }
}
