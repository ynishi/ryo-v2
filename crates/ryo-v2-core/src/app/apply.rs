//! The use case: take a set of Blueprints, verify each against the analyzer, and return the ones
//! that pass as candidates, each naming its marks and unseen classes.
//!
//! This is a dry run. Nothing is written to disk: a passing Blueprint becomes a
//! [`Candidate`] that carries the whole-file [`Edit`]s it would make.

use crate::app::ports::{CheckError, Verifier};
use crate::domain::resolution::DEFAULT_BUDGET;
use crate::domain::stage::{On, Refused, Staged, stage};
use crate::domain::{
    Blocking, Blueprint, BlueprintId, BlueprintSet, Candidate, DecisionLog, Evidence, Infeasible,
    LogEntry, NearMiss, NonEmpty, Resolution, Scope, Stage, UnseenClass, Verdict,
};
use std::collections::{BTreeMap, BTreeSet};

/// Why `apply_blueprints` could not finish.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// The analyzer check failed as a process, not as a verdict, and the failure reaches the
    /// session (`CheckReach::Session`); a failure that reaches only one Blueprint blocks it.
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
    Candidate(Box<Candidate>, Box<Staged>),
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

/// Verifies every Blueprint of `set` against the analyzer and sorts out which pass.
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
///   a candidate whose `unseen_classes` name `BuildScript` or `ProcMacroCrate` when the
///   verifier is blind to them.
///   tested: app::apply::tests::crate_graph_edits_never_pass_silently
/// - C3: a base fingerprint or a read of a file the analyzer does not have gives
///   `MissingFile`, also when other entries drifted, and the check is not called.
///   tested: app::apply::tests::missing_file_is_blocking
///   tested: app::apply::tests::read_of_file_without_base_is_checked
/// - C4: a check failure of reach `Session` returns `ApplyError::Check` at once; one of reach
///   `Blueprint` blocks that Blueprint (`Blocking::CheckFailed`, or `BudgetExhausted` for
///   `TimedOut`), logs a `Check` entry with `ok` false, and the other Blueprints are still
///   decided.
///   tested: app::apply::tests::session_check_error_is_apply_error
///   tested: app::apply::tests::per_blueprint_check_error_blocks_only_that_blueprint
/// - C5: the result is `Decided` when at least one Blueprint passed, with the candidates in
///   input order; a failed Blueprint with one to `NEAR_MISS_MAX` new errors is also listed
///   as a near miss, and a `Decided` resolution lists the blockings of the Blueprints that did
///   not pass, in input order. With no candidate the result is `Failed`.
///   tested: app::apply::tests::one_passing_of_two_is_decided_with_near_miss
///   tested: app::apply::tests::decided_lists_blockings_of_the_others
/// - C6: each stage logs one entry per Blueprint, `ok` false at the stage that blocked it and
///   no entry after that; the note of the `Candidate` entry is the candidate's `marks_line`.
///   tested: app::apply::tests::decision_log_stops_at_blocking_stage
///   tested: app::apply::tests::candidate_log_names_its_marks
/// - C7: the candidate of an intent with `Borrow` and `Behaviour` risks has both in its marks
///   and, when the verifier is blind to borrows, names `Borrow` in its `unseen_classes`.
///   tested: app::apply::tests::borrow_intent_names_borrow_unseen
/// - C9: the marks of a candidate are derived from its Blueprint's intent and the roles of the
///   files it edits.
///   tested: app::apply::tests::candidate_marks_follow_its_blueprint
/// - C10: a Blueprint with a spec that writes the bytes it replaces is blocked at the
///   conformance stage as `Nonconforming` with `Nonconformance::NoOp`, and the check is not
///   called.
///   tested: app::apply::tests::no_op_rename_is_refused_and_never_checked
/// - C11: a `Decided` resolution's evidence carries the verifier's provenance unchanged.
///   tested: app::apply::tests::evidence_carries_the_provenance
/// - C12: a Blueprint with `builds_on` is staged on its parent's staged path (`On::Parent`):
///   its reads, bases and specs are against the parent candidate's result, the check is given
///   the composed edits of the chain, its candidate's path is the chain (root first) and its
///   basis is the loaded text's (`Basis::of_chain`).
///   tested: app::apply::tests::child_builds_on_parent_result
///   tested: ryo-v2-host tests/apply.rs builds_on_chain_is_decided
/// - C13: a Blueprint whose parent is not a candidate is blocked as `ParentFailed`, logs one
///   `Freshness` entry with `ok` false, and neither the stages nor the check run for it; a
///   chain below it fails the same way.
///   tested: app::apply::tests::child_of_failed_parent_is_never_checked
/// - C14: a `Decided` resolution's evidence carries the verifier's blind classes, and each
///   candidate is given them to name the ones its marks touch.
///   tested: app::apply::tests::evidence_carries_the_checkers_blind_classes
pub fn apply_blueprints<V: Verifier>(
    v: &V,
    s: &mut V::Session,
    set: &BlueprintSet,
    scope: Scope,
) -> Result<Resolution, ApplyError> {
    let baseline = v.baseline(s);
    let provenance = v.provenance(s);
    let blind = v.blind_classes();
    let mut acc = Acc::default();
    let mut candidates = Vec::new();
    let mut blocking = Vec::new();
    let mut near_miss = Vec::new();
    let mut staged_of: BTreeMap<BlueprintId, Staged> = BTreeMap::new();
    for bp in set.iter() {
        let parent = match bp.builds_on() {
            None => None,
            Some(p) => match staged_of.get(&p) {
                Some(staged) => Some(staged),
                None => {
                    acc.note(
                        bp.id(),
                        Stage::Freshness,
                        false,
                        format!("builds on {p}, which did not pass"),
                    );
                    blocking.push(Blocking::ParentFailed {
                        blueprint: bp.id(),
                        parent: p,
                    });
                    continue;
                }
            },
        };
        match decide(
            v,
            s,
            bp,
            parent,
            &Decision {
                baseline: &baseline,
                scope,
                blind: &blind,
            },
            &mut acc,
        )? {
            Outcome::Candidate(c, staged) => {
                staged_of.insert(bp.id(), *staged);
                candidates.push(*c);
            }
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
            blocking,
            decision_log: acc.log,
            evidence: Evidence::new(
                scope,
                acc.fingerprints_checked,
                baseline.len(),
                acc.verdicts,
                DEFAULT_BUDGET,
                provenance,
                blind,
            ),
        });
    }
    // A set has at least one Blueprint and each one ends as a candidate or a blocking, so
    // `blocking` is not empty here; the fallback keeps the type honest.
    let infeasible =
        Infeasible::new(blocking, near_miss, false).unwrap_or_else(Infeasible::exhausted);
    Ok(Resolution::Failed(infeasible))
}

/// What every Blueprint of a set is decided against.
#[derive(Clone, Copy)]
struct Decision<'a> {
    baseline: &'a [crate::domain::Finding],
    scope: Scope,
    blind: &'a BTreeSet<UnseenClass>,
}

fn decide<V: Verifier>(
    v: &V,
    s: &mut V::Session,
    bp: &Blueprint,
    parent: Option<&Staged>,
    ctx: &Decision<'_>,
    acc: &mut Acc,
) -> Result<Outcome, ApplyError> {
    let Decision {
        baseline,
        scope,
        blind,
    } = *ctx;
    let id = bp.id();
    let staged = {
        let session: &V::Session = s;
        stage(
            bp,
            parent.map_or(On::Start(&[]), On::Parent),
            &|p| v.text(session, p),
            &|p| v.facts(session, p),
            &mut acc.log,
            &mut acc.fingerprints_checked,
        )
    };
    let staged = match staged {
        Ok(staged) => staged,
        Err(Refused { blocking, .. }) => {
            return Ok(Outcome::Blocked {
                blocking,
                near_miss: None,
            });
        }
    };

    // 6. check
    let found = match v.check(s, staged.edits(), scope) {
        Ok(found) => found,
        Err(e) => {
            let note = format!("check failed: {e}");
            return match e.into_blocking(id) {
                Ok(blocking) => {
                    acc.note(id, Stage::Check, false, note);
                    Ok(Outcome::Blocked {
                        blocking,
                        near_miss: None,
                    })
                }
                Err(source) => Err(ApplyError::Check {
                    blueprint: id,
                    source,
                }),
            };
        }
    };
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
    let kept = staged.clone();
    match Candidate::new(staged, verdict, blind) {
        Ok(c) => {
            acc.note(id, Stage::Candidate, true, c.marks_line());
            Ok(Outcome::Candidate(Box::new(c), Box::new(kept)))
        }
        Err(verdict) => {
            let near_miss = NearMiss::of(id, errors);
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
    use crate::domain::resolution::NEAR_MISS_MAX;
    use crate::domain::search::compose;
    use crate::domain::{
        BaseFp, BlueprintError, BlueprintParts, CheckFailure, DecisionRecord, Drift, Edit,
        ErrorCode, FileRole, Finding, FindingCode, Freshness, Ident, IntentCall, MutationSpec,
        Nonconformance, Provenance, ReadRange, RelPath, RiskMark, ScopedFindings, SpecRef,
        TextRange, UnseenClass,
    };
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeSet;
    use std::collections::{BTreeMap, VecDeque};

    #[derive(Default)]
    struct Fake {
        texts: BTreeMap<RelPath, String>,
        facts: BTreeMap<RelPath, FileFacts>,
        baseline: Vec<Finding>,
        provenance: Option<Provenance>,
        results: RefCell<VecDeque<Result<ScopedFindings, CheckError>>>,
        checks: Cell<usize>,
        seen: RefCell<Vec<Vec<Edit>>>,
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
        fn blind_classes(&self) -> BTreeSet<UnseenClass> {
            BTreeSet::from(crate::app::ALWAYS_BLIND)
        }
        fn check(
            &self,
            _: &mut (),
            edits: &[Edit],
            _: Scope,
        ) -> Result<ScopedFindings, CheckError> {
            self.checks.set(self.checks.get() + 1);
            self.seen.borrow_mut().push(edits.to_vec());
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
            builds_on: None,
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
    /// `bp` with `builds_on` set to `parent`.
    fn chained(bp: &Blueprint, parent: BlueprintId) -> Blueprint {
        Blueprint::new(BlueprintParts {
            id: bp.id(),
            intent: bp.intent().clone(),
            specs: bp.specs().to_vec(),
            reads: bp.reads().to_vec(),
            bases: bp.bases().to_vec(),
            spec_refs: bp.spec_refs().to_vec(),
            record: bp.record().clone(),
            builds_on: Some(parent),
        })
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
            ..Default::default()
        }
    }
    fn run(fake: &Fake, bps: Vec<Blueprint>) -> Result<Resolution, ApplyError> {
        apply_blueprints(fake, &mut (), &set(bps), Scope::Workspace)
    }
    fn failed(r: Result<Resolution, ApplyError>) -> Infeasible {
        match r.unwrap() {
            Resolution::Failed(i) => i,
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn child_builds_on_parent_result() {
        let path = "src/lib.rs";
        let a_text = "let t = 1 + 2;\nf(t);";
        let final_text = "let u = 1 + 2;\nf(u);";
        let a = extract(1, path);
        // B's offsets and fingerprints are those of A's result, not of the loaded text.
        let b = make(
            2,
            IntentCall::RenameLocal {
                from: Ident::parse("t").unwrap(),
                to: Ident::parse("u").unwrap(),
            },
            vec![
                MutationSpec::replace(rel(path), tr(4, 5), "u").unwrap(),
                MutationSpec::replace(rel(path), tr(17, 18), "u").unwrap(),
            ],
            path,
            a_text,
        )
        .unwrap();
        let loaded = "f(1 + 2);";
        let fake = Fake::with_text(path, loaded);
        let r = run(&fake, vec![a.clone(), chained(&b, a.id())]).unwrap();
        let Resolution::Decided { blueprints, .. } = r else {
            panic!("expected Decided");
        };
        assert_eq!(blueprints.len(), 2);
        let (ca, cb) = (&blueprints.as_slice()[0], &blueprints.as_slice()[1]);
        let a_edits = vec![Edit::write(rel(path), a_text)];
        assert_eq!(ca.edits(), a_edits.as_slice());
        let expected = vec![Edit::write(rel(path), final_text)];
        assert_eq!(cb.edits(), expected.as_slice());
        let own = vec![Edit::write(rel(path), final_text)];
        let base = |p: &RelPath| fake.texts.get(p).cloned();
        assert_eq!(cb.edits(), compose(&a_edits, &own, &base).as_slice());
        assert_eq!(fake.seen.borrow().last(), Some(&expected));
        let ids: Vec<_> = cb.path().iter().map(Blueprint::id).collect();
        assert_eq!(ids, vec![bid(1), bid(2)]);
        assert_eq!(
            cb.basis().files(),
            [BaseFp::capture(rel(path), loaded)].as_slice()
        );
        assert_eq!(fake.checks.get(), 2);

        // Without `builds_on` the same Blueprint is stale against the loaded text.
        let fake = Fake::with_text(path, loaded);
        let r = run(&fake, vec![a.clone(), b]).unwrap();
        let Resolution::Decided { blueprints, .. } = r else {
            panic!("expected Decided");
        };
        assert_eq!(blueprints.len(), 1);
        assert_eq!(fake.checks.get(), 1);
        let fake = Fake::with_text(path, loaded);
        let alone = failed(run(
            &fake,
            vec![
                make(
                    2,
                    IntentCall::RenameLocal {
                        from: Ident::parse("t").unwrap(),
                        to: Ident::parse("u").unwrap(),
                    },
                    vec![MutationSpec::replace(rel(path), tr(4, 5), "u").unwrap()],
                    path,
                    a_text,
                )
                .unwrap(),
            ],
        ));
        assert!(matches!(alone.blocking()[0], Blocking::Stale { .. }));
        assert_eq!(fake.checks.get(), 0);

        // A child on another file is staged on the parent it names, not on the candidate
        // before it, and its check is given the edits of the whole chain, not its own alone.
        let mut fake = Fake::with_text(path, loaded);
        fake.texts.insert(rel("src/x.rs"), BASE.to_owned());
        fake.texts.insert(rel("src/other.rs"), BASE.to_owned());
        let x = rename(3, "src/x.rs");
        let c = chained(&rename(4, "src/other.rs"), a.id());
        let r = run(&fake, vec![a.clone(), x, c]).unwrap();
        let Resolution::Decided { blueprints, .. } = r else {
            panic!("expected Decided");
        };
        assert_eq!(blueprints.len(), 3);
        let cc = &blueprints.as_slice()[2];
        let ids: Vec<_> = cc.path().iter().map(Blueprint::id).collect();
        assert_eq!(ids, vec![bid(1), bid(4)]);
        let chain = vec![
            Edit::write(rel("src/lib.rs"), a_text),
            Edit::write(rel("src/other.rs"), "let b = 1;"),
        ];
        assert_eq!(cc.edits(), chain.as_slice());
        assert_eq!(fake.seen.borrow().last(), Some(&chain));
        assert_eq!(
            cc.basis().files(),
            [
                BaseFp::capture(rel("src/lib.rs"), loaded),
                BaseFp::capture(rel("src/other.rs"), BASE)
            ]
            .as_slice()
        );
    }

    #[test]
    fn child_of_failed_parent_is_never_checked() {
        let fake = Fake::with_text("src/lib.rs", BASE);
        fake.queue(Ok(errors(1)));
        let a = rename(1, "src/lib.rs");
        let b = chained(&rename(2, "src/lib.rs"), a.id());
        let c = chained(&rename(3, "src/lib.rs"), b.id());
        // D has no parent and passes, so the set is decided and its log is kept.
        let d = rename(4, "src/lib.rs");
        let r = run(&fake, vec![a.clone(), b, c, d]).unwrap();
        let Resolution::Decided {
            blueprints,
            decision_log,
            ..
        } = r
        else {
            panic!("expected Decided");
        };
        // Only A (failed) and D were checked.
        assert_eq!(fake.checks.get(), 2);
        assert_eq!(blueprints.len(), 1);
        assert_eq!(blueprints.first().blueprint(), bid(4));
        for n in [2, 3] {
            let entries: Vec<_> = decision_log
                .entries()
                .iter()
                .filter(|e| e.blueprint() == bid(n))
                .collect();
            assert_eq!(entries.len(), 1, "{n}");
            assert_eq!(
                (entries[0].stage(), entries[0].ok()),
                (Stage::Freshness, false)
            );
        }
        // Without a passing sibling the blockings name the failed parents.
        let fake = Fake::with_text("src/lib.rs", BASE);
        fake.queue(Ok(errors(1)));
        let b = chained(&rename(2, "src/lib.rs"), a.id());
        let c = chained(&rename(3, "src/lib.rs"), b.id());
        let i = failed(run(&fake, vec![a, b, c]));
        assert_eq!(fake.checks.get(), 1);
        assert!(matches!(i.blocking()[0], Blocking::NewErrors { .. }));
        assert_eq!(
            i.blocking()[1..],
            [
                Blocking::ParentFailed {
                    blueprint: bid(2),
                    parent: bid(1)
                },
                Blocking::ParentFailed {
                    blueprint: bid(3),
                    parent: bid(2)
                },
            ]
        );
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
        let i = failed(run(&fake, vec![same]));
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
        let i = failed(run(&fake, vec![rename(1, "src/lib.rs")]));
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
            builds_on: None,
        })
        .unwrap();
        // The bytes of the read are unchanged, but the file is not the base any more.
        let moved = "fn f() {\n    let a = 1;\n    let a = 1;\n}\n";
        assert_eq!(bp.reads()[0].check(moved), Freshness::Fresh);
        let fake = Fake::with_text("src/lib.rs", moved);
        let i = failed(run(&fake, vec![bp]));
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
            builds_on: None,
        })
        .unwrap();
        let mut fake = Fake::with_text("src/lib.rs", BASE);
        // the file that is only read is missing
        let i = failed(run(&fake, vec![bp.clone()]));
        assert_eq!(
            i.blocking(),
            &[Blocking::MissingFile {
                blueprint: bid(1),
                path: other.clone()
            }]
        );
        // missing wins over drift elsewhere: the edited file changed as well
        fake.texts
            .insert(rel("src/lib.rs"), "let a = 2;".to_owned());
        let i = failed(run(&fake, vec![bp.clone()]));
        assert_eq!(fake.checks.get(), 0);
        assert_eq!(
            i.blocking(),
            &[Blocking::MissingFile {
                blueprint: bid(1),
                path: other.clone()
            }]
        );
        fake.texts.insert(rel("src/lib.rs"), BASE.to_owned());
        // it changed
        fake.texts.insert(other.clone(), "abd".to_owned());
        let i = failed(run(&fake, vec![bp.clone()]));
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
        let r = run(&fake, vec![bp]).unwrap();
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
                builds_on: None,
            })
            .unwrap()
        };
        let mut fake = Fake::with_text("src/a.rs", "x let a = 1;");
        fake.texts.insert(b.clone(), "let a = 1; y".to_owned());
        fake.texts.insert(c.clone(), "abd".to_owned());
        let i = failed(run(&fake, vec![bp(1)]));
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
        let r = run(&fake, vec![bp(1), rename(2, "src/ok.rs")]).unwrap();
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
        let r = run(&fake, vec![rename(1, "src/lib.rs")]).unwrap();
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
        let r = run(&fake, vec![rename(1, "src/lib.rs"), second]).unwrap();
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
            builds_on: None,
        })
        .unwrap();
        let mut fake = Fake::with_text("src/a.rs", BASE);
        fake.texts.insert(rel("src/c.rs"), BASE.to_owned());
        let r = run(&fake, vec![bp]).unwrap();
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
        )
        .unwrap();
        assert_eq!(fake.checks.get(), 3);
        let Resolution::Decided {
            blueprints,
            decision_log,
            evidence,
            ..
        } = r
        else {
            panic!("expected Decided");
        };
        assert_eq!(blueprints.len(), 1);
        let c = blueprints.first();
        assert_eq!(c.blueprint(), bid(1));
        assert_eq!(c.edits(), &[Edit::write(rel("src/lib.rs"), "let b = 1;")]);
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
        ));
        assert_eq!(i.blocking().len(), 2);
        assert!(matches!(i.blocking()[0], Blocking::NewErrors { .. }));
        assert_eq!(i.near_miss(), &[NearMiss::of(bid(2), 1).unwrap()]);
    }

    #[test]
    fn borrow_intent_names_borrow_unseen() {
        let fake = Fake::with_text("src/lib.rs", "f(1 + 2);");
        let r = run(&fake, vec![extract(1, "src/lib.rs")]).unwrap();
        let Resolution::Decided { blueprints, .. } = r else {
            panic!("expected Decided");
        };
        let c = blueprints.first();
        assert_eq!(
            c.marks(),
            &BTreeSet::from([RiskMark::Borrow, RiskMark::Behaviour])
        );
        assert_eq!(
            c.unseen_classes().unwrap().classes(),
            &BTreeSet::from([UnseenClass::Borrow])
        );
        assert_eq!(c.edits().len(), 1);
    }

    #[test]
    fn candidate_log_names_its_marks() {
        let mut fake = Fake::with_text("src/lib.rs", BASE);
        fake.texts.insert(rel("src/x.rs"), "f(1 + 2);".to_owned());
        let r = run(&fake, vec![rename(1, "src/lib.rs"), extract(2, "src/x.rs")]).unwrap();
        let Resolution::Decided { decision_log, .. } = r else {
            panic!("expected Decided");
        };
        let note = |id| {
            decision_log
                .entries()
                .iter()
                .find(|e| e.blueprint() == id && e.stage() == Stage::Candidate)
                .unwrap()
                .note()
                .to_owned()
        };
        assert_eq!(note(bid(1)), "marks: none");
        assert_eq!(note(bid(2)), "marks: borrow, behaviour");
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
            builds_on: None,
        })
        .unwrap()
    }

    #[test]
    fn candidate_marks_follow_its_blueprint() {
        let mut fake = Fake::with_text("src/lib.rs", BASE);
        fake.texts.insert(rel("src/x.rs"), "f(1 + 2);".to_owned());
        fake.texts
            .insert(rel("src/a.rs"), "pub fn foo() {}\n".to_owned());
        fake.texts.insert(rel("src/b.rs"), String::new());
        let r = run(
            &fake,
            vec![rename(1, "src/lib.rs"), extract(2, "src/x.rs"), move_foo(3)],
        )
        .unwrap();
        let Resolution::Decided { blueprints, .. } = r else {
            panic!("expected Decided");
        };
        let cs = blueprints.as_slice();
        assert_eq!(cs.len(), 3);
        assert_eq!(cs[0].marks(), &BTreeSet::new());
        assert!(cs[0].unseen_classes().is_none());
        assert_eq!(
            cs[1].marks(),
            &BTreeSet::from([RiskMark::Borrow, RiskMark::Behaviour])
        );
        assert_eq!(
            cs[1].unseen_classes().unwrap().classes(),
            &BTreeSet::from([UnseenClass::Borrow])
        );
        assert_eq!(cs[2].marks(), &BTreeSet::from([RiskMark::Visibility]));
        assert_eq!(
            cs[2].unseen_classes().unwrap().classes(),
            &BTreeSet::from([UnseenClass::Visibility])
        );
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
        let r = run(&fake, vec![rename(1, "src/lib.rs")]).unwrap();
        let Resolution::Decided { evidence, .. } = r else {
            panic!("expected Decided");
        };
        assert_eq!(evidence.provenance(), &given);
    }

    #[test]
    fn evidence_carries_the_checkers_blind_classes() {
        let fake = Fake::with_text("src/lib.rs", BASE);
        let r = run(&fake, vec![rename(1, "src/lib.rs")]).unwrap();
        let Resolution::Decided {
            evidence,
            blueprints,
            ..
        } = r
        else {
            panic!("expected Decided");
        };
        assert_eq!(
            evidence.blind_classes(),
            &BTreeSet::from(crate::app::ALWAYS_BLIND)
        );
        // A blind class no mark of the candidate names is in the evidence only.
        assert!(blueprints.as_slice()[0].unseen_classes().is_none());
    }

    #[test]
    fn missing_file_is_blocking() {
        let fake = Fake::default();
        let i = failed(run(&fake, vec![rename(1, "src/lib.rs")]));
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
    fn session_check_error_is_apply_error() {
        let session_errors = [
            CheckError::Poisoned("x".to_owned()),
            CheckError::ForkInUse,
            CheckError::RevertMismatch {
                paths: vec![rel("src/lib.rs")],
            },
        ];
        for err in session_errors {
            let fake = Fake::with_text("src/lib.rs", BASE);
            fake.queue(Err(err.clone()));
            let e = run(
                &fake,
                vec![rename(1, "src/lib.rs"), rename(2, "src/lib.rs")],
            )
            .unwrap_err();
            match e {
                ApplyError::Check { blueprint, source } => {
                    assert_eq!(blueprint, bid(1));
                    assert_eq!(source, err);
                }
            }
            // The second Blueprint was not checked.
            assert_eq!(fake.checks.get(), 1);
        }
    }

    #[test]
    fn per_blueprint_check_error_blocks_only_that_blueprint() {
        let failures = [
            (
                CheckError::Panicked("boom".to_owned()),
                CheckFailure::Panicked("boom".to_owned()),
            ),
            (
                CheckError::Edit {
                    path: rel("src/lib.rs"),
                    reason: "no".to_owned(),
                },
                CheckFailure::Edit {
                    path: rel("src/lib.rs"),
                    reason: "no".to_owned(),
                },
            ),
        ];
        for (err, failure) in failures {
            let fake = Fake::with_text("src/lib.rs", BASE);
            fake.queue(Err(err));
            fake.queue(Ok(ScopedFindings::default()));
            let r = run(
                &fake,
                vec![rename(1, "src/lib.rs"), rename(2, "src/lib.rs")],
            )
            .unwrap();
            assert_eq!(fake.checks.get(), 2);
            let Resolution::Decided {
                blueprints,
                blocking,
                decision_log,
                ..
            } = r
            else {
                panic!("expected Decided");
            };
            assert_eq!(blueprints.len(), 1);
            assert_eq!(blueprints.first().blueprint(), bid(2));
            assert_eq!(
                blocking,
                vec![Blocking::CheckFailed {
                    blueprint: bid(1),
                    failure
                }]
            );
            let check = decision_log
                .entries()
                .iter()
                .find(|e| e.blueprint() == bid(1) && e.stage() == Stage::Check)
                .unwrap();
            assert!(!check.ok());
        }
    }

    #[test]
    fn decided_lists_blockings_of_the_others() {
        let fake = Fake::with_text("src/lib.rs", BASE);
        fake.queue(Ok(ScopedFindings::default()));
        fake.queue(Ok(errors(1)));
        let a = rename(1, "src/lib.rs");
        let b = rename(2, "src/lib.rs");
        let c = chained(&rename(3, "src/lib.rs"), b.id());
        let r = run(&fake, vec![a, b, c]).unwrap();
        let Resolution::Decided {
            blueprints,
            blocking,
            ..
        } = r
        else {
            panic!("expected Decided");
        };
        assert_eq!(blueprints.len(), 1);
        assert_eq!(blocking.len(), 2);
        assert!(matches!(
            &blocking[0],
            Blocking::NewErrors { blueprint, .. } if *blueprint == bid(2)
        ));
        assert_eq!(
            blocking[1],
            Blocking::ParentFailed {
                blueprint: bid(3),
                parent: bid(2)
            }
        );
    }

    #[test]
    fn decision_log_stops_at_blocking_stage() {
        let mut fake = Fake::with_text("src/lib.rs", BASE);
        fake.texts
            .insert(rel("src/other.rs"), "let a = 2;".to_owned());
        let r = run(
            &fake,
            vec![rename(1, "src/other.rs"), rename(2, "src/lib.rs")],
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
        // A manifest is not verifiable, whatever the facts say, and is never checked.
        let mut fake = Fake::with_text("Cargo.toml", BASE);
        fake.facts.insert(
            rel("Cargo.toml"),
            FileFacts {
                is_build_script: true,
                in_proc_macro_crate: true,
            },
        );
        let i = failed(run(&fake, vec![rename(1, "Cargo.toml")]));
        assert_eq!(fake.checks.get(), 0);
        assert_eq!(
            i.blocking(),
            &[Blocking::Unverifiable {
                blueprint: bid(1),
                path: rel("Cargo.toml"),
                role: FileRole::Manifest
            }]
        );

        // A build script and a proc-macro file pass the check and name their unseen class.
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
        )
        .unwrap();
        let Resolution::Decided { blueprints, .. } = r else {
            panic!("expected Decided");
        };
        let cs = blueprints.as_slice();
        assert_eq!(cs.len(), 2);
        assert!(
            cs[0]
                .unseen_classes()
                .unwrap()
                .classes()
                .contains(&UnseenClass::BuildScript)
        );
        assert!(
            cs[1]
                .unseen_classes()
                .unwrap()
                .classes()
                .contains(&UnseenClass::ProcMacroCrate)
        );
    }
}
