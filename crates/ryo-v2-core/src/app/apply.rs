//! The use case: take a set of Blueprints, verify each against the analyzer, and say which
//! may be applied and how.
//!
//! This is a dry run. Nothing is written to disk: a passing Blueprint becomes a
//! [`Candidate`] that carries the whole-file [`Edit`]s it would make.

use crate::app::ports::{CheckError, Verifier};
use crate::domain::file_role::file_role;
use crate::domain::resolution::{DEFAULT_BUDGET, NEAR_MISS_MAX};
use crate::domain::text::splice;
use crate::domain::{
    Blocking, Blueprint, BlueprintId, BlueprintSet, Candidate, DecisionLog, Edit, Evidence,
    FileRole, Freshness, Infeasible, LogEntry, NearMiss, NonEmpty, ReadRange, RelPath, Resolution,
    RiskMark, RuleCatalog, Scope, Stage, Verdict,
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

#[derive(Default)]
struct Acc {
    log: DecisionLog,
    verdicts: Vec<(BlueprintId, Verdict)>,
    fingerprints_checked: usize,
}

impl Acc {
    fn note(&mut self, blueprint: BlueprintId, stage: Stage, ok: bool, note: impl Into<String>) {
        self.log.push(LogEntry {
            blueprint,
            stage,
            ok,
            note: note.into(),
        });
    }
}

fn blocked(blocking: Blocking) -> Outcome {
    Outcome::Blocked {
        blocking,
        near_miss: None,
    }
}

/// Verifies every Blueprint of `set` against the analyzer and sorts out which may be applied.
///
/// # Contract
/// - C1: a Blueprint with a read that is not fresh is blocked by one `Blocking::Stale` that
///   lists every non-fresh read, and `Verifier::check` is not called for it.
///   tested: app::apply::tests::stale_blueprint_is_failed_and_never_checked
/// - C2: a spec in a file whose role is `Manifest` is blocked as `Unverifiable` before
///   anything is read, and the check is not called; a build script or proc-macro file gives
///   a candidate whose `outer_check` is `Some`, which is never `AutoApplicable`.
///   tested: app::apply::tests::crate_graph_edits_never_pass_silently
/// - C3: a read of a file the analyzer does not have gives `MissingFile`, and the check is
///   not called.
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
pub fn apply_blueprints<V: Verifier>(
    v: &V,
    s: &mut V::Session,
    set: &BlueprintSet,
    rules: &RuleCatalog,
    scope: Scope,
) -> Result<Resolution, ApplyError> {
    let baseline = v.baseline(s);
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
            evidence: Evidence {
                scope,
                fingerprints_checked: acc.fingerprints_checked,
                baseline_size: baseline.len(),
                verdicts: acc.verdicts,
                budget: DEFAULT_BUDGET,
            },
        });
    }
    // A set has at least one Blueprint and each one ends as a candidate or a blocking, so
    // `blocking` is not empty here; the fallback keeps the type honest.
    let infeasible =
        Infeasible::new(blocking, near_miss, false).unwrap_or_else(Infeasible::exhausted);
    Ok(Resolution::Failed(infeasible))
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
    let by_file = bp.edits_by_file();

    // 1. file roles
    let mut roles = Vec::new();
    for path in by_file.keys() {
        let role = file_role(path, &v.facts(s, path));
        if role == FileRole::Manifest {
            acc.note(id, Stage::FileRole, false, format!("{path} is a {role}"));
            return Ok(blocked(Blocking::Unverifiable {
                blueprint: id,
                path: (*path).clone(),
                role,
            }));
        }
        roles.push(role);
    }
    acc.note(
        id,
        Stage::FileRole,
        true,
        format!("{} file(s)", roles.len()),
    );

    // 2. freshness
    let mut stale: Vec<(ReadRange, Freshness)> = Vec::new();
    for read in bp.reads() {
        let Some(current) = v.text(s, read.path()) else {
            acc.note(
                id,
                Stage::Freshness,
                false,
                format!("{} is not in the workspace", read.path()),
            );
            return Ok(blocked(Blocking::MissingFile {
                blueprint: id,
                path: read.path().clone(),
            }));
        };
        acc.fingerprints_checked += 1;
        match read.check(&current) {
            Freshness::Fresh => {}
            other => stale.push((read.clone(), other)),
        }
    }
    if !stale.is_empty() {
        acc.note(
            id,
            Stage::Freshness,
            false,
            format!("{} read(s) are not fresh", stale.len()),
        );
        return Ok(blocked(Blocking::Stale {
            blueprint: id,
            reads: stale,
        }));
    }
    acc.note(
        id,
        Stage::Freshness,
        true,
        format!("{} read(s) fresh", bp.reads().len()),
    );

    // 3. conformance
    let base: BTreeMap<RelPath, String> = by_file
        .keys()
        .filter_map(|p| v.text(s, p).map(|t| ((*p).clone(), t)))
        .collect();
    if let Err(why) = bp.intent().conforms(bp.specs(), &base) {
        acc.note(id, Stage::Conformance, false, why.to_string());
        return Ok(blocked(Blocking::Nonconforming { blueprint: id, why }));
    }
    acc.note(id, Stage::Conformance, true, bp.intent().to_string());

    // 4. splice
    let mut edits = Vec::new();
    for (path, specs) in &by_file {
        let Some(text) = base.get(*path) else {
            // Cannot happen after the freshness stage; reported as a missing file all the same.
            acc.note(id, Stage::Splice, false, format!("{path} has no text"));
            return Ok(blocked(Blocking::MissingFile {
                blueprint: id,
                path: (*path).clone(),
            }));
        };
        match splice(text, specs) {
            Ok(new_text) => edits.push(Edit::write((*path).clone(), new_text)),
            Err(err) => {
                acc.note(id, Stage::Splice, false, err.to_string());
                return Ok(blocked(Blocking::Splice { blueprint: id, err }));
            }
        }
    }
    acc.note(id, Stage::Splice, true, format!("{} edit(s)", edits.len()));

    // 5. marks
    let kind = bp.intent().kind();
    let mut marks: BTreeSet<RiskMark> = kind.spec().risks;
    for role in &roles {
        match role {
            FileRole::BuildScript => {
                marks.insert(RiskMark::BuildScript);
            }
            FileRole::ProcMacroSource => {
                marks.insert(RiskMark::ProcMacroCrate);
            }
            FileRole::Source | FileRole::Manifest => {}
        }
    }
    let names: Vec<String> = marks.iter().map(ToString::to_string).collect();
    acc.note(id, Stage::Marks, true, names.join(", "));

    // 6. check
    let found = v
        .check(s, &edits, scope)
        .map_err(|source| ApplyError::Check {
            blueprint: id,
            source,
        })?;
    let verdict = Verdict::from_baseline(baseline, &found);
    acc.verdicts.push((id, verdict.clone()));
    let errors = verdict.new_errors.errors.len() + verdict.new_errors.unmapped.len();
    acc.note(
        id,
        Stage::Check,
        verdict.passed(),
        format!("{errors} new error(s)"),
    );

    // 7. candidate
    match Candidate::new(id, edits, verdict, marks, rules.for_intent(kind), scope) {
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
        BlueprintError, DecisionRecord, Disposition, ErrorCode, Finding, FindingCode, Ident,
        IntentCall, IntentKind, MutationSpec, OuterCheckReason, ReviewReason, ReviewRecord, Rule,
        RuleId, ScopedFindings, SpecRef, TextRange,
    };
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    #[derive(Default)]
    struct Fake {
        texts: BTreeMap<RelPath, String>,
        facts: BTreeMap<RelPath, FileFacts>,
        baseline: Vec<Finding>,
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
        Blueprint::new(
            bid(n),
            intent,
            specs,
            vec![ReadRange::capture(rel(path), text, tr(0, end)).unwrap()],
            vec![SpecRef::new("t").unwrap()],
            DecisionRecord::new("agent", "why").unwrap(),
        )
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
    fn rename_rule() -> RuleCatalog {
        RuleCatalog::new(vec![
            Rule::register(
                RuleId::parse("01890a5d-ac96-774b-bcce-b302099a8099").unwrap(),
                IntentKind::RenameLocal,
                ReviewRecord {
                    reviewer: "me".to_owned(),
                    reviewed_at_ms: 1,
                    note: String::new(),
                },
                Scope::Affected,
            )
            .unwrap(),
        ])
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
            Blocking::Stale { blueprint, reads } => {
                assert_eq!(*blueprint, bid(1));
                assert_eq!(reads.len(), 1);
                assert!(matches!(reads[0].1, Freshness::Stale { .. }));
            }
            other => panic!("{other:?}"),
        }
        assert!(!i.budget_exhausted());
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
            &rename_rule(),
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
            Disposition::AutoApplicable {
                rule: RuleId::parse("01890a5d-ac96-774b-bcce-b302099a8099").unwrap()
            }
        );
        assert_eq!(evidence.verdicts.len(), 3);
        assert_eq!(evidence.fingerprints_checked, 3);
        assert_eq!(evidence.baseline_size, 0);
        assert_eq!(evidence.scope, Scope::Workspace);
        assert_eq!(evidence.budget, DEFAULT_BUDGET);
        let last_of = |n| {
            decision_log
                .entries()
                .iter()
                .rfind(|e| e.blueprint == bid(n))
                .unwrap()
                .clone()
        };
        assert_eq!((last_of(1).stage, last_of(1).ok), (Stage::Candidate, true));
        assert_eq!((last_of(2).stage, last_of(2).ok), (Stage::Check, false));

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
        for rules in [RuleCatalog::default(), rename_rule()] {
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
                .filter(|e| e.blueprint == bid(n))
                .map(|e| (e.stage, e.ok))
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
        let rules = rename_rule();
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
        assert!(matches!(
            cs[0].disposition(),
            Disposition::AfterOuterCheck { .. }
        ));
        assert_eq!(
            cs[1].disposition(),
            Disposition::NeedsReview {
                reason: ReviewReason::ProcMacroCrate
            }
        );
    }
}
