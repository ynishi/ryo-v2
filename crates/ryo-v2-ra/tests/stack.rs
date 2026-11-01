//! The levels of a fork, and the checks and discards built on them, on throwaway workspaces (the
//! corpus is never touched).

mod common;

use common::{Fixture, rel};
use ryo_v2_core::app::ports::{
    Analyzer, AnalyzerError, CheckError, Event, ExpandError, Expander, RevertAudit, Verifier,
};
use ryo_v2_core::domain::baseline::new_findings;
use ryo_v2_core::domain::{Edit, Scope, Target, UnseenClass, Verdict};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_ra::{GapDetector, RaAnalyzer, RaConfig, RaSession};
use std::sync::Arc;

/// One package whose `src/lib.rs` has an E0599 and a name only the unresolved-path detector
/// reports.
const UNRESOLVED_LIB: &str =
    "pub struct S;\npub fn g(s: S) {\n    s.missing();\n}\npub fn f(_x: Missing) {}\n";

fn lib() -> ryo_v2_core::domain::RelPath {
    rel("src/lib.rs")
}

/// An analyzer on the config the tests of this file share; `assert_revert` turns the diagnostic
/// re-assert of every revert on.
fn analyzer(assert_revert: bool) -> (RaAnalyzer, Arc<RecordingObserver>) {
    let observer = Arc::new(RecordingObserver::default());
    let a = RaAnalyzer::new(
        RaConfig {
            proc_macro: false,
            threads: Some(1),
            assert_revert,
            gap_detectors: true,
            assert_revert_every: None,
            cfg_test: false,
        },
        Arc::new(MonotonicClock::new()),
        observer.clone(),
    );
    (a, observer)
}

fn lib_edit(extra: &str) -> Edit {
    Edit::write(lib(), format!("{UNRESOLVED_LIB}{extra}"))
}

fn loaded(test: &str, a: &RaAnalyzer) -> (Fixture, RaSession) {
    let f = Fixture::single(test, UNRESOLVED_LIB);
    let s = Analyzer::load(a, &f.dir).expect("load fixture");
    (f, s)
}

fn checked_or_reverted(events: &[Event]) -> Vec<&Event> {
    events
        .iter()
        .filter(|e| matches!(e, Event::Checked { .. } | Event::Reverted { .. }))
        .collect()
}

#[test]
fn an_empty_check_diagnoses_nothing() {
    let (a, _) = analyzer(false);
    // Loaded with the fault, so that the baseline holds the panic and a scan of `src/lib.rs`
    // would name the class of the detector.
    let a = a.with_detector_fault(Some((GapDetector::UnresolvedPath, lib())));
    let (_f, mut s) = loaded("stack_empty_check", &a);

    let found = a
        .check(&mut s, &[], Scope::Affected)
        .expect("an empty check is Ok");
    assert!(found.files.is_empty(), "no file diagnosed: {found:?}");
    assert!(
        found.unseen.is_empty(),
        "no class named: {:?}",
        found.unseen
    );
    assert!(found.findings.is_empty(), "{:?}", found.findings);
    let baseline = Verifier::baseline(&a, &s);
    assert!(Verdict::from_baseline(&baseline, &found).passed());
    assert!(new_findings(&baseline, &found).is_empty());

    // The control: a scan of the whole workspace sees the file and names the class.
    let all = a
        .check(&mut s, &[], Scope::Workspace)
        .expect("a workspace scan");
    assert_eq!(all.files, vec![lib()]);
    assert!(
        all.unseen.contains(&UnseenClass::UnresolvedPath),
        "{:?}",
        all.unseen
    );
}

#[test]
fn text_is_the_base_after_a_poisoned_check() {
    let (a, _) = analyzer(false);
    let a = a.with_revert_fault(Some(lib()));
    let (_f, mut s) = loaded("stack_text_after_poison", &a);
    let provenance_before = a.provenance(&s);
    let fp_before = provenance_before.files().get(&lib()).copied();
    assert!(fp_before.is_some(), "src/lib.rs is a fingerprinted file");

    let err = a
        .check(&mut s, &[lib_edit("\npub fn h() {}\n")], Scope::Affected)
        .expect_err("the revert leaves another text");
    assert_eq!(err, CheckError::RevertMismatch { paths: vec![lib()] });
    // The database holds the wrong text of the revert; the reads still describe the base.
    assert_eq!(a.text(&s, &lib()).as_deref(), Some(UNRESOLVED_LIB));
    assert_eq!(a.provenance(&s).files().get(&lib()).copied(), fp_before);
    let next = a.check(&mut s, &[], Scope::Affected);
    assert!(matches!(next, Err(CheckError::Poisoned(_))), "{next:?}");
    assert_eq!(a.text(&s, &lib()).as_deref(), Some(UNRESOLVED_LIB));
}

#[test]
fn a_session_error_of_the_release_wins_in_a_check() {
    let (healthy, _) = analyzer(true);
    let (faulty, _) = analyzer(true);
    // The diagnosis is cut off at once, and the re-assert after the revert lacks the finding of
    // the detector that the baseline has.
    let faulty = faulty
        .with_detector_fault(Some((GapDetector::UnresolvedPath, lib())))
        .with_check_timeout(Some(0));
    let (_f, mut s) = loaded("stack_release_wins", &healthy);

    let first = faulty.check(&mut s, &[lib_edit("\npub fn h() {}\n")], Scope::Affected);
    assert!(
        matches!(first, Err(CheckError::RevertMismatch { .. })),
        "{first:?}"
    );
    let second = healthy.check(&mut s, &[lib_edit("")], Scope::Affected);
    assert!(matches!(second, Err(CheckError::Poisoned(_))), "{second:?}");
}

#[test]
fn analyzer_discard_compares_texts() {
    let (a, _) = analyzer(false);
    let a = a.with_revert_fault(Some(lib()));
    let (_f, mut s) = loaded("stack_discard_texts", &a);

    let mut fork = Analyzer::fork(&a, &mut s).expect("fork");
    Analyzer::apply(&a, &mut s, &mut fork, &[lib_edit("\npub fn h() {}\n")]).expect("apply");
    let discarded = Analyzer::discard(&a, &mut s, fork);
    assert!(
        matches!(discarded, Err(AnalyzerError::RevertMismatch)),
        "{discarded:?}"
    );
    let next = Analyzer::fork(&a, &mut s);
    assert!(next.is_err(), "a poisoned session must refuse a fork");
}

#[test]
fn a_refused_check_reverts_nothing() {
    let (a, obs) = analyzer(false);
    let (_f, mut s) = loaded("stack_refused_check", &a);
    obs.take();

    let manifest = Edit::write(rel("Cargo.toml"), "[package]\n");
    let err = a
        .check(&mut s, &[manifest], Scope::Affected)
        .expect_err("a manifest is not an indexed file");
    assert!(matches!(err, CheckError::Edit { .. }), "{err:?}");
    let events = obs.take();
    assert!(
        checked_or_reverted(&events).is_empty(),
        "a refused check records no check or revert: {events:?}"
    );
    assert_eq!(a.text(&s, &lib()).as_deref(), Some(UNRESOLVED_LIB));

    // The next check works, and records both.
    a.check(&mut s, &[lib_edit("\npub fn h() {}\n")], Scope::Affected)
        .expect("the next check");
    let events = obs.take();
    assert!(
        matches!(
            checked_or_reverted(&events).as_slice(),
            [Event::Checked { .. }, Event::Reverted { .. }]
        ),
        "{events:?}"
    );
}

#[test]
fn an_analyzer_ticket_of_another_session_is_refused() {
    let (a, _) = analyzer(false);
    let (_fa, mut sa) = loaded("stack_ticket_a", &a);
    let (_fb, mut sb) = loaded("stack_ticket_b", &a);
    let mut fork_a = Analyzer::fork(&a, &mut sa).expect("fork a");
    let mut fork_b = Analyzer::fork(&a, &mut sb).expect("fork b");
    let edit = lib_edit("\npub fn h() {}\n");
    let refused = |r: Result<(), AnalyzerError>| matches!(r, Err(AnalyzerError::ForkInUse));

    // The other session's ticket lands on nothing.
    assert!(refused(Analyzer::apply(
        &a,
        &mut sa,
        &mut fork_b,
        std::slice::from_ref(&edit)
    )));
    assert!(refused(
        Analyzer::diagnostics(&a, &sa, &fork_b, Scope::Affected).map(|_| ())
    ));
    assert!(refused(Analyzer::discard(&a, &mut sa, fork_b.clone())));
    assert!(refused(Analyzer::apply(
        &a,
        &mut sb,
        &mut fork_a,
        std::slice::from_ref(&edit)
    )));
    assert!(refused(Analyzer::discard(&a, &mut sb, fork_a.clone())));
    for s in [&sa, &sb] {
        assert_eq!(a.text(s, &lib()).as_deref(), Some(UNRESOLVED_LIB));
    }

    // Each session's own fork was not touched: it still takes its one write, and ends.
    Analyzer::apply(&a, &mut sa, &mut fork_a, std::slice::from_ref(&edit)).expect("apply a");
    Analyzer::diagnostics(&a, &sa, &fork_a, Scope::Affected).expect("diagnostics a");
    let stale_a = fork_a.clone();
    Analyzer::discard(&a, &mut sa, fork_a).expect("discard a");
    Analyzer::apply(&a, &mut sb, &mut fork_b, std::slice::from_ref(&edit)).expect("apply b");
    Analyzer::discard(&a, &mut sb, fork_b).expect("discard b");
    for s in [&sa, &sb] {
        assert_eq!(a.text(s, &lib()).as_deref(), Some(UNRESOLVED_LIB));
    }

    // A discarded ticket is refused, also while another fork of its session is live.
    let mut stale = stale_a.clone();
    assert!(refused(Analyzer::apply(
        &a,
        &mut sa,
        &mut stale,
        std::slice::from_ref(&edit)
    )));
    assert!(refused(
        Analyzer::diagnostics(&a, &sa, &stale_a, Scope::Affected).map(|_| ())
    ));
    assert!(refused(Analyzer::discard(&a, &mut sa, stale_a.clone())));
    let fresh = Analyzer::fork(&a, &mut sa).expect("a new fork of the session");
    assert!(refused(Analyzer::discard(&a, &mut sa, stale_a)));
    Analyzer::discard(&a, &mut sa, fresh).expect("the new fork ends");
}

#[test]
fn a_poisoned_fork_is_not_written() {
    let (a, _) = analyzer(false);
    let (_f, mut s) = loaded("stack_poisoned_fork", &a);
    let mut fork = Analyzer::fork(&a, &mut s).expect("fork");
    // As a panic of an earlier write of this fork would.
    s.poison("test");

    let applied = Analyzer::apply(&a, &mut s, &mut fork, &[lib_edit("\npub fn h() {}\n")]);
    assert!(
        matches!(applied, Err(AnalyzerError::Panicked(_))),
        "{applied:?}"
    );
    let diagnosed = Analyzer::diagnostics(&a, &s, &fork, Scope::Affected);
    assert!(
        matches!(diagnosed, Err(AnalyzerError::Panicked(_))),
        "{diagnosed:?}"
    );
    assert_eq!(a.text(&s, &lib()).as_deref(), Some(UNRESOLVED_LIB));
    let discarded = Analyzer::discard(&a, &mut s, fork);
    assert!(discarded.is_err(), "{discarded:?}");
    assert_eq!(a.text(&s, &lib()).as_deref(), Some(UNRESOLVED_LIB));
}

#[test]
fn a_live_analyzer_fork_holds_the_session() {
    let (a, _) = analyzer(false);
    let (_f, mut s) = loaded("stack_live_fork_holds", &a);
    let mut fork = Analyzer::fork(&a, &mut s).expect("fork");
    Analyzer::apply(&a, &mut s, &mut fork, &[lib_edit("\npub fn h() {}\n")]).expect("apply");

    assert_eq!(a.text(&s, &lib()).as_deref(), Some(UNRESOLVED_LIB));
    let checked = a.check(&mut s, &[lib_edit("")], Scope::Affected);
    assert_eq!(checked, Err(CheckError::ForkInUse));
    let expanded = a.expand(&mut s, &[], &[Target::new(lib(), None)], 10);
    assert!(
        matches!(expanded, Err(ExpandError::Session(CheckError::ForkInUse))),
        "{expanded:?}"
    );
    assert_eq!(a.audit_revert(&mut s), Err(CheckError::ForkInUse));

    Analyzer::discard(&a, &mut s, fork).expect("discard");
    a.check(&mut s, &[lib_edit("")], Scope::Affected)
        .expect("a check once the fork has ended");
    assert_eq!(a.text(&s, &lib()).as_deref(), Some(UNRESOLVED_LIB));
}
