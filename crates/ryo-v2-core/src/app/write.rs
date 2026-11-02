//! The use case: check a chosen candidate before its edits are written.
//!
//! Nothing is written here. [`prepare_write`] checks that the files are still what the candidate's
//! basis says, that the session has read the current text, and that the edits still pass the
//! check on it; what it returns, a [`CheckedWrite`], is what a writer writes.

use crate::app::ports::{CheckError, Verifier};
use crate::app::verdict_of;
use crate::domain::basis::View;
use crate::domain::resolution::named_unseen;
use crate::domain::{
    BlueprintId, Drift, Edit, Fingerprint, RelPath, Resolution, UnseenClasses, Verdict,
};
use std::collections::BTreeSet;

/// Why the check before a write refused.
#[derive(Debug, thiserror::Error)]
pub enum WriteRefusal {
    /// The chosen Blueprint is not a candidate of the decision.
    #[error("{0} is not a candidate of this decision")]
    NotACandidate(BlueprintId),
    /// More than one candidate of the decision ends in the chosen Blueprint.
    #[error("{0} ends more than one candidate of this decision")]
    AmbiguousPick(BlueprintId),
    /// The session's provenance lists no file, so what the session has read cannot be compared
    /// with the current text.
    #[error(
        "the session's provenance lists no file; its text cannot be compared with the current text"
    )]
    UnrecordedSession,
    /// The files no longer match the candidate's basis.
    #[error("the files no longer match the basis of the candidate ({} drift(s))", .0.len())]
    Drift(Vec<Drift>),
    /// An edit is on a file that the basis does not hold and that exists now.
    #[error("{0} is edited, is not in the basis of the candidate and exists now")]
    Appeared(RelPath),
    /// The session has not read the current text of these files.
    #[error(
        "the session has not read the current text of: {}",
        .0.iter().map(RelPath::as_str).collect::<Vec<_>>().join(", ")
    )]
    StaleSession(Vec<RelPath>),
    /// The edits give new errors on the current text.
    #[error(
        "the edits give {} new error(s) on the current text",
        .0.new_errors.len()
    )]
    NewErrors(Verdict),
    /// The re-check did not finish.
    #[error("the re-check did not finish: {0}")]
    Check(#[source] CheckError),
}

/// The edits of a candidate that passed the check before a write, with the verdict of the
/// re-check and the unseen classes it names.
///
/// ```compile_fail
/// use ryo_v2_core::app::write::CheckedWrite;
/// use ryo_v2_core::domain::{Diagnostics, Verdict};
/// // Only `prepare_write` builds one: its fields are private.
/// let _ = CheckedWrite {
///     edits: vec![],
///     verdict: Verdict::from_diagnostics(Diagnostics::default()),
///     unseen_classes: None,
/// };
/// ```
///
/// # Contract
/// - C1 [invariant]: every value comes from `prepare_write` after the basis, the created paths
///   and the session have been checked and the edits passed a re-check on the current text.
///   tested: app::write::tests::clean_recheck_returns_the_edits
///   tested: app::write::tests::drifted_candidate_is_refused_and_never_checked
///   tested: app::write::tests::edit_creating_a_file_that_now_exists_is_refused
///   tested: app::write::tests::stale_session_is_refused_and_never_checked
///   tested: app::write::tests::new_error_on_the_current_text_refuses_the_write
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedWrite {
    edits: Vec<Edit>,
    verdict: Verdict,
    unseen_classes: Option<UnseenClasses>,
}

impl CheckedWrite {
    /// The whole-file edits to write.
    pub fn edits(&self) -> &[Edit] {
        &self.edits
    }

    /// The verdict of the re-check on the current text.
    pub fn verdict(&self) -> &Verdict {
        &self.verdict
    }

    /// The classes of error the checker does not see that this write names: those its candidate's
    /// marks name that the checker is blind to now, and those the re-check observed.
    pub fn unseen_classes(&self) -> Option<&UnseenClasses> {
        self.unseen_classes.as_ref()
    }
}

/// Checks the candidate of `decided` whose Blueprint is `pick` before its edits are written.
///
/// `current` is the text of the files as they are now. The scope of the re-check is the scope the
/// candidate was decided at; the caller does not give one.
///
/// # Contract
/// - C1: a candidate whose basis has drifted against `current` is refused with
///   `WriteRefusal::Drift`, listing every drift, and `Verifier::check` is not called.
///   tested: app::write::tests::drifted_candidate_is_refused_and_never_checked
/// - C2: an edit on a path that is not in the basis and that `current` has is refused with
///   `WriteRefusal::Appeared`, and `Verifier::check` is not called.
///   tested: app::write::tests::edit_creating_a_file_that_now_exists_is_refused
/// - C3: a file of the session's provenance that `current` lacks, or whose fingerprint differs
///   from the fingerprint of its current text, refuses with `WriteRefusal::StaleSession`, listing
///   every such file, and `Verifier::check` is not called. A provenance that lists no file refuses
///   with `WriteRefusal::UnrecordedSession` (what the session read cannot be compared), and
///   `Verifier::check` is not called.
///   tested: app::write::tests::stale_session_is_refused_and_never_checked
///   tested: app::write::tests::unrecorded_provenance_is_refused
/// - C4: a re-check whose verdict does not pass is refused with `WriteRefusal::NewErrors`; a
///   re-check that fails is refused with `WriteRefusal::Check`.
///   tested: app::write::tests::new_error_on_the_current_text_refuses_the_write
/// - C5: a candidate that passes every check gives a `CheckedWrite` of its edits and the verdict
///   of the re-check.
///   tested: app::write::tests::clean_recheck_returns_the_edits
/// - C6: the unseen classes of the `CheckedWrite` are those its candidate's marks name that the
///   checker is blind to when it is written, together with those the re-check observed.
///   tested: app::write::tests::recheck_names_the_unseen_it_observed
/// - C7: the checks run in the order basis, created paths, session, re-check, and the first that
///   refuses is the refusal given.
///   tested: app::write::tests::drift_wins_over_a_stale_session
/// - C8: the re-check runs at the scope the decision's evidence carries.
///   tested: app::write::tests::recheck_uses_the_decided_scope
/// - C9: a `pick` that is no candidate's Blueprint, or a `decided` that is not `Decided`, is
///   refused with `WriteRefusal::NotACandidate`; a `pick` that more than one candidate ends in is
///   refused with `WriteRefusal::AmbiguousPick`, so the edits returned are never another
///   candidate's.
///   tested: app::write::tests::unknown_pick_is_refused
///   tested: app::write::tests::ambiguous_pick_is_refused
pub fn prepare_write<V: Verifier>(
    verifier: &V,
    session: &mut V::Session,
    decided: &Resolution,
    pick: BlueprintId,
    current: &View<'_>,
) -> Result<CheckedWrite, WriteRefusal> {
    let Resolution::Decided {
        blueprints,
        evidence,
        ..
    } = decided
    else {
        return Err(WriteRefusal::NotACandidate(pick));
    };
    let mut ending = blueprints.iter().filter(|c| c.blueprint() == pick);
    let candidate = ending.next().ok_or(WriteRefusal::NotACandidate(pick))?;
    if ending.next().is_some() {
        return Err(WriteRefusal::AmbiguousPick(pick));
    }

    let drift = candidate.basis().check(current);
    if !drift.is_empty() {
        return Err(WriteRefusal::Drift(drift));
    }

    let in_basis: BTreeSet<&RelPath> = candidate
        .basis()
        .files()
        .iter()
        .map(|b| b.path())
        .chain(candidate.basis().reads().iter().map(|r| r.path()))
        .collect();
    if let Some(edit) = candidate
        .edits()
        .iter()
        .find(|e| !in_basis.contains(&e.path) && current(&e.path).is_some())
    {
        return Err(WriteRefusal::Appeared(edit.path.clone()));
    }

    let provenance = verifier.provenance(session);
    if provenance.files().is_empty() {
        return Err(WriteRefusal::UnrecordedSession);
    }
    let stale: Vec<RelPath> = provenance
        .files()
        .iter()
        .filter(|(path, fingerprint)| {
            current(path).is_none_or(|text| Fingerprint::of(text.as_bytes()) != **fingerprint)
        })
        .map(|(path, _)| path.clone())
        .collect();
    if !stale.is_empty() {
        return Err(WriteRefusal::StaleSession(stale));
    }

    let baseline = verifier.baseline(session);
    let verdict = verdict_of(
        verifier,
        session,
        candidate.edits(),
        evidence.scope(),
        &baseline,
    )
    .map_err(WriteRefusal::Check)?;
    if !verdict.passed() {
        return Err(WriteRefusal::NewErrors(verdict));
    }
    let unseen_classes = named_unseen(
        candidate.marks(),
        &verifier.blind_classes(),
        &verdict.unseen,
    );
    Ok(CheckedWrite {
        edits: candidate.edits().to_vec(),
        verdict,
        unseen_classes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::apply::apply_blueprints;
    use crate::app::ports::FileFacts;
    use crate::domain::resolution::DEFAULT_BUDGET;
    use crate::domain::stage::Staged;
    use crate::domain::{
        BaseFp, Blueprint, BlueprintParts, BlueprintSet, Candidate, Command, DecisionLog,
        DecisionRecord, Diagnostics, ErrorCode, Evidence, Finding, FindingCode, Freshness, Ident,
        Infeasible, NonEmpty, Provenance, ReadRange, Scope, ScopedFindings, SpecRef, Splice,
        TextRange, UnseenClass,
    };
    use std::cell::{Cell, RefCell};
    use std::collections::{BTreeMap, VecDeque};

    struct Fake {
        texts: BTreeMap<RelPath, String>,
        baseline: Vec<Finding>,
        blind: RefCell<BTreeSet<UnseenClass>>,
        results: RefCell<VecDeque<Result<ScopedFindings, CheckError>>>,
        checks: Cell<usize>,
        scopes: RefCell<Vec<Scope>>,
    }

    impl Verifier for Fake {
        type Session = ();
        fn text(&self, _: &(), path: &RelPath) -> Option<String> {
            self.texts.get(path).cloned()
        }
        fn facts(&self, _: &(), _: &RelPath) -> FileFacts {
            FileFacts::default()
        }
        fn baseline(&self, _: &()) -> Vec<Finding> {
            self.baseline.clone()
        }
        fn provenance(&self, _: &()) -> Provenance {
            let files = self
                .texts
                .iter()
                .map(|(p, t)| (p.clone(), Fingerprint::of(t.as_bytes())))
                .collect();
            Provenance::new("fake", "0", BTreeMap::new(), files).unwrap()
        }
        fn blind_classes(&self) -> BTreeSet<UnseenClass> {
            self.blind.borrow().clone()
        }
        fn check(
            &self,
            _: &mut (),
            _: &[Edit],
            scope: Scope,
        ) -> Result<ScopedFindings, CheckError> {
            self.checks.set(self.checks.get() + 1);
            self.scopes.borrow_mut().push(scope);
            self.results
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Ok(ScopedFindings::default()))
        }
    }

    impl Fake {
        fn new(items: &[(&str, &str)]) -> Self {
            Self {
                texts: texts(items),
                baseline: vec![],
                blind: RefCell::new(BTreeSet::from(crate::app::ALWAYS_BLIND)),
                results: RefCell::new(VecDeque::new()),
                checks: Cell::new(0),
                scopes: RefCell::new(vec![]),
            }
        }
        fn queue(&self, r: Result<ScopedFindings, CheckError>) {
            self.results.borrow_mut().push_back(r);
        }
    }

    const BASE: &str = "let a = 1;";
    const LIB: &str = "src/lib.rs";
    const OTHER: &str = "src/other.rs";

    fn rel(s: &str) -> RelPath {
        RelPath::parse(s).unwrap()
    }
    fn bid(n: u8) -> BlueprintId {
        BlueprintId::parse(&format!("01890a5d-ac96-774b-bcce-b302099a80{n:02x}")).unwrap()
    }
    fn tr(a: u32, b: u32) -> TextRange {
        TextRange::new(a, b).unwrap()
    }
    fn texts(items: &[(&str, &str)]) -> BTreeMap<RelPath, String> {
        items
            .iter()
            .map(|(p, t)| (rel(p), (*t).to_owned()))
            .collect()
    }
    fn view(texts: &BTreeMap<RelPath, String>) -> impl Fn(&RelPath) -> Option<String> + '_ {
        move |p| texts.get(p).cloned()
    }
    fn make(n: u8, command: Command, splices: Vec<Splice>, text: &str) -> Blueprint {
        let end = u32::try_from(text.len()).unwrap();
        Blueprint::new(BlueprintParts {
            id: bid(n),
            command,
            splices,
            reads: vec![ReadRange::capture(rel(LIB), text, tr(0, end)).unwrap()],
            bases: vec![BaseFp::capture(rel(LIB), text)],
            spec_refs: vec![SpecRef::parse("spec#t").unwrap()],
            record: DecisionRecord::new("agent", "why").unwrap(),
            builds_on: None,
        })
        .unwrap()
    }
    /// Renames `a` to `b` in `BASE`.
    fn rename(n: u8) -> Blueprint {
        make(
            n,
            Command::RenameLocal {
                from: Ident::parse("a").unwrap(),
                to: Ident::parse("b").unwrap(),
            },
            vec![Splice::replace(rel(LIB), tr(4, 5), "b").unwrap()],
            BASE,
        )
    }
    /// Extracts `1 + 2` of `f(1 + 2);`: a change that carries the `Borrow` mark.
    fn extract(n: u8) -> Blueprint {
        make(
            n,
            Command::ExtractVariable {
                name: Ident::parse("t").unwrap(),
            },
            vec![
                Splice::insert(rel(LIB), 0, "let t = 1 + 2;\n").unwrap(),
                Splice::replace(rel(LIB), tr(2, 7), "t").unwrap(),
            ],
            "f(1 + 2);",
        )
    }
    fn errors(n: usize) -> ScopedFindings {
        ScopedFindings {
            files: vec![rel(LIB)],
            findings: (0..n)
                .map(|i| Finding {
                    code: FindingCode::Rustc(ErrorCode::parse("E0599").unwrap()),
                    message: format!("m{i}"),
                    path: rel(LIB),
                    line: 1,
                })
                .collect(),
            ..Default::default()
        }
    }
    fn observed(class: UnseenClass) -> ScopedFindings {
        ScopedFindings {
            unseen_at: BTreeMap::from([(class, BTreeSet::new())]),
            ..Default::default()
        }
    }
    fn decide(fake: &Fake, bps: Vec<Blueprint>, scope: Scope) -> Resolution {
        apply_blueprints(fake, &mut (), &BlueprintSet::new(bps).unwrap(), scope).unwrap()
    }
    fn write(
        fake: &Fake,
        decided: &Resolution,
        pick: BlueprintId,
        current: &BTreeMap<RelPath, String>,
    ) -> Result<CheckedWrite, WriteRefusal> {
        prepare_write(fake, &mut (), decided, pick, &view(current))
    }
    /// A candidate of `bp` with `edits`, with the basis of `bp`.
    fn candidate_of(bp: &Blueprint, edits: Vec<Edit>) -> Candidate {
        let staged = Staged::for_tests(
            NonEmpty::one(bp.clone()),
            BTreeSet::new(),
            edits,
            bp.basis(),
        );
        Candidate::new(
            staged,
            Verdict::from_diagnostics(Diagnostics::default()),
            &BTreeSet::new(),
        )
        .unwrap()
    }
    fn decided_of(candidates: Vec<Candidate>, scope: Scope) -> Resolution {
        Resolution::Decided {
            blueprints: NonEmpty::new(candidates).unwrap(),
            blocking: vec![],
            decision_log: DecisionLog::default(),
            evidence: Evidence::new(
                scope,
                0,
                0,
                vec![],
                DEFAULT_BUDGET,
                Provenance::unrecorded(),
                BTreeSet::new(),
            ),
        }
    }

    #[test]
    fn clean_recheck_returns_the_edits() {
        let fake = Fake::new(&[(LIB, BASE), (OTHER, "fn o() {}")]);
        let r = decide(&fake, vec![rename(1)], Scope::Workspace);
        assert_eq!(fake.checks.get(), 1);
        let w = write(&fake, &r, bid(1), &fake.texts).unwrap();
        assert_eq!(w.edits(), [Edit::write(rel(LIB), "let b = 1;")].as_slice());
        assert!(w.verdict().passed());
        assert_eq!(w.unseen_classes(), None);
        // the decision's check and the re-check
        assert_eq!(fake.checks.get(), 2);
    }

    #[test]
    fn drifted_candidate_is_refused_and_never_checked() {
        let fake = Fake::new(&[(LIB, BASE)]);
        let r = decide(&fake, vec![rename(1)], Scope::Workspace);
        let current = texts(&[(LIB, "let a = 2;")]);
        let Err(WriteRefusal::Drift(drift)) = write(&fake, &r, bid(1), &current) else {
            panic!("expected Drift");
        };
        assert_eq!(drift.len(), 1);
        assert!(matches!(
            &drift[0],
            Drift::File { base, now: Freshness::Stale { .. } } if base.path() == &rel(LIB)
        ));
        assert_eq!(fake.checks.get(), 1);
    }

    #[test]
    fn edit_creating_a_file_that_now_exists_is_refused() {
        let fake = Fake::new(&[(LIB, BASE)]);
        let bp = rename(1);
        let created = rel("src/new.rs");
        let r = decided_of(
            vec![candidate_of(
                &bp,
                vec![
                    Edit::write(rel(LIB), "let b = 1;"),
                    Edit::write(created.clone(), "fn n() {}"),
                ],
            )],
            Scope::Workspace,
        );
        let mut current = fake.texts.clone();
        current.insert(created.clone(), "fn other() {}".to_owned());
        let refused = write(&fake, &r, bid(1), &current);
        assert!(
            matches!(&refused, Err(WriteRefusal::Appeared(p)) if *p == created),
            "{refused:?}"
        );
        assert_eq!(fake.checks.get(), 0);
        // Without the file there, the same candidate is written.
        let w = write(&fake, &r, bid(1), &fake.texts).unwrap();
        assert_eq!(w.edits().len(), 2);
        assert_eq!(fake.checks.get(), 1);
    }

    #[test]
    fn stale_session_is_refused_and_never_checked() {
        let fake = Fake::new(&[(LIB, BASE), (OTHER, "fn o() {}")]);
        let r = decide(&fake, vec![rename(1)], Scope::Workspace);
        // A file outside the basis changed since the session read it.
        let mut changed = fake.texts.clone();
        changed.insert(rel(OTHER), "fn o() { }".to_owned());
        let refused = write(&fake, &r, bid(1), &changed);
        assert!(
            matches!(&refused, Err(WriteRefusal::StaleSession(p)) if *p == [rel(OTHER)]),
            "{refused:?}"
        );
        // A file the session read is gone.
        let mut gone = fake.texts.clone();
        gone.remove(&rel(OTHER));
        let refused = write(&fake, &r, bid(1), &gone);
        assert!(
            matches!(&refused, Err(WriteRefusal::StaleSession(p)) if *p == [rel(OTHER)]),
            "{refused:?}"
        );
        assert_eq!(fake.checks.get(), 1);
    }

    #[test]
    fn new_error_on_the_current_text_refuses_the_write() {
        let mut fake = Fake::new(&[(LIB, BASE), (OTHER, "fn o() {}")]);
        let r = decide(&fake, vec![rename(1)], Scope::Workspace);
        // A file outside the basis changed and the session has read it; the edits now give an
        // error on that text.
        fake.texts.insert(rel(OTHER), "fn o() { }".to_owned());
        fake.queue(Ok(errors(1)));
        let refused = write(&fake, &r, bid(1), &fake.texts);
        let Err(WriteRefusal::NewErrors(verdict)) = refused else {
            panic!("expected NewErrors, got {refused:?}");
        };
        assert_eq!(verdict.new_errors.errors.len(), 1);
        assert_eq!(fake.checks.get(), 2);
        // A re-check that fails is refused as such.
        fake.queue(Err(CheckError::Panicked("boom".to_owned())));
        let refused = write(&fake, &r, bid(1), &fake.texts);
        assert!(
            matches!(&refused, Err(WriteRefusal::Check(CheckError::Panicked(m))) if m == "boom"),
            "{refused:?}"
        );
    }

    #[test]
    fn recheck_names_the_unseen_it_observed() {
        let fake = Fake::new(&[(LIB, "f(1 + 2);")]);
        let r = decide(&fake, vec![extract(1)], Scope::Workspace);
        let Resolution::Decided { blueprints, .. } = &r else {
            panic!("expected Decided");
        };
        // Decided while the checker was blind to borrows: the candidate names them.
        assert_eq!(
            blueprints.first().unseen_classes().unwrap().classes(),
            &BTreeSet::from([UnseenClass::Borrow])
        );
        let named = |w: Result<CheckedWrite, WriteRefusal>| {
            w.unwrap().unseen_classes().map(|u| u.classes().clone())
        };
        // Still blind when it is written: named again.
        assert_eq!(
            named(write(&fake, &r, bid(1), &fake.texts)),
            Some(BTreeSet::from([UnseenClass::Borrow]))
        );
        // The checker sees borrows when it is written: the mark names nothing.
        *fake.blind.borrow_mut() = BTreeSet::new();
        assert_eq!(named(write(&fake, &r, bid(1), &fake.texts)), None);
        // What the re-check observed is named whatever the checker is blind to.
        fake.queue(Ok(observed(UnseenClass::UnexpandedMacro)));
        assert_eq!(
            named(write(&fake, &r, bid(1), &fake.texts)),
            Some(BTreeSet::from([UnseenClass::UnexpandedMacro]))
        );
        // A class the checker became blind to since the decision is named by the mark.
        *fake.blind.borrow_mut() = BTreeSet::from([UnseenClass::Borrow]);
        fake.queue(Ok(observed(UnseenClass::UnexpandedMacro)));
        assert_eq!(
            named(write(&fake, &r, bid(1), &fake.texts)),
            Some(BTreeSet::from([
                UnseenClass::Borrow,
                UnseenClass::UnexpandedMacro
            ]))
        );
    }

    #[test]
    fn drift_wins_over_a_stale_session() {
        let fake = Fake::new(&[(LIB, BASE), (OTHER, "fn o() {}")]);
        let r = decide(&fake, vec![rename(1)], Scope::Workspace);
        let mut current = fake.texts.clone();
        current.insert(rel(LIB), "let a = 2;".to_owned());
        current.insert(rel(OTHER), "fn o() { }".to_owned());
        let refused = write(&fake, &r, bid(1), &current);
        assert!(
            matches!(refused, Err(WriteRefusal::Drift(_))),
            "{refused:?}"
        );
        assert_eq!(fake.checks.get(), 1);

        // A created path that exists now wins over a stale session.
        let created = rel("src/new.rs");
        let r = decided_of(
            vec![candidate_of(
                &rename(1),
                vec![Edit::write(created.clone(), "fn n() {}")],
            )],
            Scope::Workspace,
        );
        let mut current = fake.texts.clone();
        current.insert(created.clone(), "fn other() {}".to_owned());
        current.insert(rel(OTHER), "fn o() { }".to_owned());
        let refused = write(&fake, &r, bid(1), &current);
        assert!(
            matches!(&refused, Err(WriteRefusal::Appeared(p)) if *p == created),
            "{refused:?}"
        );
        assert_eq!(fake.checks.get(), 1);
    }

    #[test]
    fn recheck_uses_the_decided_scope() {
        for scope in [Scope::Affected, Scope::Workspace] {
            let fake = Fake::new(&[(LIB, BASE)]);
            let r = decide(&fake, vec![rename(1)], scope);
            write(&fake, &r, bid(1), &fake.texts).unwrap();
            assert_eq!(*fake.scopes.borrow(), vec![scope, scope]);
        }
    }

    #[test]
    fn unknown_pick_is_refused() {
        let fake = Fake::new(&[(LIB, BASE)]);
        let r = decide(&fake, vec![rename(1)], Scope::Workspace);
        let refused = write(&fake, &r, bid(9), &fake.texts);
        assert!(
            matches!(&refused, Err(WriteRefusal::NotACandidate(id)) if *id == bid(9)),
            "{refused:?}"
        );
        let failed = Resolution::Failed(Infeasible::exhausted());
        let refused = write(&fake, &failed, bid(1), &fake.texts);
        assert!(
            matches!(&refused, Err(WriteRefusal::NotACandidate(id)) if *id == bid(1)),
            "{refused:?}"
        );
        assert_eq!(fake.checks.get(), 1);
    }

    #[test]
    fn ambiguous_pick_is_refused() {
        let fake = Fake::new(&[(LIB, BASE)]);
        let bp = rename(1);
        let r = decided_of(
            vec![
                candidate_of(&bp, vec![Edit::write(rel(LIB), "let b = 1;")]),
                candidate_of(&bp, vec![Edit::write(rel(LIB), "let c = 1;")]),
            ],
            Scope::Workspace,
        );
        let refused = write(&fake, &r, bid(1), &fake.texts);
        assert!(
            matches!(&refused, Err(WriteRefusal::AmbiguousPick(id)) if *id == bid(1)),
            "{refused:?}"
        );
        assert_eq!(fake.checks.get(), 0);
    }

    #[test]
    fn unrecorded_provenance_is_refused() {
        // The fake's provenance lists the files it holds: with none, it lists none, while the
        // current text still has the candidate's file.
        let fake = Fake::new(&[]);
        let bp = rename(1);
        let r = decided_of(
            vec![candidate_of(&bp, vec![Edit::write(rel(LIB), "let b = 1;")])],
            Scope::Workspace,
        );
        let current = texts(&[(LIB, BASE)]);
        let refused = write(&fake, &r, bid(1), &current);
        assert!(
            matches!(&refused, Err(WriteRefusal::UnrecordedSession)),
            "{refused:?}"
        );
        assert_eq!(fake.checks.get(), 0);
    }

    #[test]
    fn decide_and_write_paths_give_the_same_verdict() {
        // The baseline accounts for the one error the check reports, so the verdict passes only
        // when the baseline is compared; the check also observes a class.
        let mut fake = Fake::new(&[(LIB, BASE)]);
        fake.baseline = errors(1).findings;
        let found = ScopedFindings {
            unseen_at: BTreeMap::from([(UnseenClass::UnexpandedMacro, BTreeSet::new())]),
            ..errors(1)
        };
        fake.queue(Ok(found.clone()));
        fake.queue(Ok(found));
        let r = decide(&fake, vec![rename(1)], Scope::Workspace);
        let Resolution::Decided { blueprints, .. } = &r else {
            panic!("expected Decided");
        };
        let decided = blueprints.first().verdict();
        assert!(decided.passed());
        assert_eq!(
            decided.unseen,
            BTreeSet::from([UnseenClass::UnexpandedMacro])
        );
        let w = write(&fake, &r, bid(1), &fake.texts).unwrap();
        assert_eq!(w.verdict(), decided);
        assert_eq!(fake.checks.get(), 2);
    }
}
