//! `Verifier::check` (a one-level fork private to the adapter), the `Analyzer` port, the flat
//! reference check and the revert audit, on throwaway workspaces (the corpus is never touched).
//! The writes that stack, jobs on Layers, are `Forks`'s (tests/forks.rs).

mod common;

use common::{Fixture, rel};
use ryo_v2_core::app::ports::{
    Analyzer, AnalyzerError, CheckError, CheckReach, Event, ExpandError, Expander, RevertAudit,
    Verifier,
};
use ryo_v2_core::domain::baseline::new_findings;
use ryo_v2_core::domain::{Edit, RelPath, Scope, Target, UnseenClass, Verdict};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_ra::{DbWritePanic, GapDetector, RaAnalyzer, RaConfig, RaSession};
use std::sync::Arc;

/// One package whose `src/lib.rs` has an E0599 and a name only the unresolved-path detector
/// reports.
const UNRESOLVED_LIB: &str =
    "pub struct S;\npub fn g(s: S) {\n    s.missing();\n}\npub fn f(_x: Missing) {}\n";

fn lib() -> RelPath {
    rel("src/lib.rs")
}

/// An analyzer on the config the tests of this file share; `assert_revert` samples every write.
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
            ..Default::default()
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

fn revert_oks(events: &[Event]) -> Vec<Option<bool>> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Reverted { revert_ok, .. } => Some(*revert_ok),
            _ => None,
        })
        .collect()
}

#[test]
fn an_empty_check_diagnoses_nothing() {
    let (a, _) = analyzer(false);
    // Loaded with the fault, so that the baseline holds the panic and a scan of `src/lib.rs`
    // would name the class of the detector.
    let a = a.with_detector_fault(Some((GapDetector::UnresolvedPath, lib())));
    let (_f, mut s) = loaded("check_empty_check", &a);

    let found = a
        .check(&mut s, &[], Scope::Affected)
        .expect("an empty check is Ok");
    assert!(found.files.is_empty(), "no file diagnosed: {found:?}");
    assert!(
        found.unseen().is_empty(),
        "no class named: {:?}",
        found.unseen()
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
        all.unseen().contains(&UnseenClass::UnresolvedPath),
        "{:?}",
        all.unseen()
    );
}

#[test]
fn text_is_the_base_after_a_poisoned_check() {
    let (a, _) = analyzer(false);
    let a = a.with_revert_fault(Some(lib()));
    let (_f, mut s) = loaded("check_text_after_poison", &a);
    let provenance_before = a.provenance(&s);
    let fp_before = provenance_before.files().get(&lib()).copied();
    assert!(fp_before.is_some(), "src/lib.rs is a fingerprinted file");

    // A check has no revert to get wrong: it passes, and the session stays usable and at its
    // base.
    a.check(&mut s, &[lib_edit("\npub fn h() {}\n")], Scope::Affected)
        .expect("a check writes nothing back");
    assert_eq!(a.text(&s, &lib()).as_deref(), Some(UNRESOLVED_LIB));
    assert_eq!(a.provenance(&s).files().get(&lib()).copied(), fp_before);
    a.check(&mut s, &[], Scope::Affected)
        .expect("the session stays usable");

    // The flat reference check writes the base and writes it back: a write-back that leaves
    // another text poisons the session (`reference_check` C3).
    let err = a
        .reference_check(&mut s, &[lib_edit("\npub fn h() {}\n")], Scope::Affected)
        .expect_err("the write-back leaves another text");
    assert_eq!(err, CheckError::RevertMismatch { paths: vec![lib()] });
    // The database holds the wrong text of the write-back; the reads still describe the base.
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
    let (_f, mut s) = loaded("check_release_wins", &healthy);

    let first = faulty.check(&mut s, &[lib_edit("\npub fn h() {}\n")], Scope::Affected);
    // The release checks the isolation of the base, which a detector fault does not touch: it
    // succeeds, and the cut-off of the diagnosis is the result.
    assert_eq!(first, Err(CheckError::TimedOut { limit_ms: 0 }));
    healthy
        .check(&mut s, &[lib_edit("")], Scope::Affected)
        .expect("the session stays usable");

    // In the flat reference check the write-back's re-assert lacks the finding of the detector
    // that the baseline has: that session error wins over the cut-off.
    let reference =
        faulty.reference_check(&mut s, &[lib_edit("\npub fn h() {}\n")], Scope::Affected);
    assert!(
        matches!(reference, Err(CheckError::RevertMismatch { .. })),
        "{reference:?}"
    );
    let second = healthy.check(&mut s, &[lib_edit("")], Scope::Affected);
    assert!(matches!(second, Err(CheckError::Poisoned(_))), "{second:?}");
}

#[test]
fn analyzer_discard_writes_nothing_back() {
    let (a, _) = analyzer(false);
    let a = a.with_revert_fault(Some(lib()));
    let (_f, mut s) = loaded("check_discard_texts", &a);

    let mut fork = Analyzer::fork(&a, &mut s).expect("fork");
    Analyzer::apply(&a, &mut s, &mut fork, &[lib_edit("\npub fn h() {}\n")]).expect("apply");
    let discarded = Analyzer::discard(&a, &mut s, fork);
    // There is no revert to get wrong: the discard drops the layer.
    discarded.expect("a discard writes nothing back");
    assert_eq!(a.text(&s, &lib()).as_deref(), Some(UNRESOLVED_LIB));
    let next = Analyzer::fork(&a, &mut s).expect("the session stays usable");
    Analyzer::discard(&a, &mut s, next).expect("discard");
}

#[test]
fn a_refused_check_reverts_nothing() {
    let (a, obs) = analyzer(false);
    let (_f, mut s) = loaded("check_refused_check", &a);
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
    let (_fa, mut sa) = loaded("check_ticket_a", &a);
    let (_fb, mut sb) = loaded("check_ticket_b", &a);
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
    let (_f, mut s) = loaded("check_poisoned_fork", &a);
    let mut fork = Analyzer::fork(&a, &mut s).expect("fork");
    // As a panic of an earlier write of this fork would.
    s.poison("test");

    let applied = Analyzer::apply(&a, &mut s, &mut fork, &[lib_edit("\npub fn h() {}\n")]);
    assert!(
        matches!(applied, Err(AnalyzerError::Poisoned(_))),
        "{applied:?}"
    );
    let diagnosed = Analyzer::diagnostics(&a, &s, &fork, Scope::Affected);
    assert!(
        matches!(diagnosed, Err(AnalyzerError::Poisoned(_))),
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
    let (_f, mut s) = loaded("check_live_fork_holds", &a);
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

#[test]
fn a_revert_that_cannot_be_confirmed_poisons_the_session() {
    // The isolation check at the end of a sampled check cannot run: the session stops, and the
    // reads still describe the base.
    let (a, _) = analyzer(true);
    let a = a.with_reassert_fault(true);
    let (_f, mut s) = loaded("check_reassert_cannot_run", &a);
    let checked = a.check(&mut s, &[lib_edit("\npub fn h() {}\n")], Scope::Affected);
    assert!(
        matches!(&checked, Err(CheckError::Poisoned(m)) if m.contains("could not run")),
        "{checked:?}"
    );
    assert_eq!(a.text(&s, &lib()).as_deref(), Some(UNRESOLVED_LIB));
    let next = a.check(&mut s, &[], Scope::Affected);
    assert!(matches!(next, Err(CheckError::Poisoned(_))), "{next:?}");
}

#[test]
fn a_database_write_that_panics_poisons_the_session() {
    let (healthy, _) = analyzer(false);

    // Through a check: the write is a layer of its own, so its panic is a Blueprint error, and
    // the session is usable.
    let (c, _) = analyzer(false);
    let c = c.with_db_write_panic(Some(DbWritePanic::Write));
    let (_g, mut t) = loaded("check_db_write_panic", &c);
    let checked = c.check(&mut t, &[lib_edit("\npub fn h() {}\n")], Scope::Affected);
    assert!(
        matches!(&checked, Err(e @ CheckError::Panicked(_)) if e.reach() == CheckReach::Blueprint),
        "{checked:?}"
    );
    assert_eq!(c.text(&t, &lib()).as_deref(), Some(UNRESOLVED_LIB));
    healthy
        .check(&mut t, &[lib_edit("")], Scope::Affected)
        .expect("the session stays usable");

    // The flat reference check writes the base in place: a panic of its write, or of its
    // write-back, poisons the session, with the reads at the base.
    for (at, test) in [
        (DbWritePanic::Write, "check_db_write_panic_reference"),
        (DbWritePanic::Revert, "check_db_revert_panic_reference"),
    ] {
        let (p, _) = analyzer(false);
        let p = p.with_db_write_panic(Some(at));
        let (_i, mut v) = loaded(test, &p);
        let checked = p.reference_check(&mut v, &[lib_edit("\npub fn h() {}\n")], Scope::Affected);
        assert!(
            matches!(&checked, Err(e @ CheckError::Poisoned(_)) if e.reach() == CheckReach::Session),
            "{at:?}: {checked:?}"
        );
        assert_eq!(
            p.text(&v, &lib()).as_deref(),
            Some(UNRESOLVED_LIB),
            "{at:?}"
        );
        let next = healthy.check(&mut v, &[], Scope::Affected);
        assert!(
            matches!(next, Err(CheckError::Poisoned(_))),
            "{at:?}: {next:?}"
        );
    }
}

#[test]
fn the_flat_reference_reasserts_its_level() {
    // A healthy write-back: the re-assert runs and agrees with the baseline.
    let (healthy, obs) = analyzer(true);
    let (_f, mut s) = loaded("check_flat_reassert", &healthy);
    obs.take();
    healthy
        .reference_check(&mut s, &[lib_edit("\npub fn h() {}\n")], Scope::Affected)
        .expect("a healthy reference check");
    assert_eq!(revert_oks(&obs.take()), vec![Some(true)]);

    // The re-assert lacks the finding of the detector that the baseline has: `RevertMismatch` of
    // the edited paths, the session poisoned, the reads at the base.
    let (faulty, _) = analyzer(true);
    let faulty = faulty.with_detector_fault(Some((GapDetector::UnresolvedPath, lib())));
    let checked = faulty.reference_check(&mut s, &[lib_edit("\npub fn h() {}\n")], Scope::Affected);
    assert_eq!(
        checked,
        Err(CheckError::RevertMismatch { paths: vec![lib()] })
    );
    assert_eq!(healthy.text(&s, &lib()).as_deref(), Some(UNRESOLVED_LIB));
    let next = healthy.check(&mut s, &[], Scope::Affected);
    assert!(matches!(next, Err(CheckError::Poisoned(_))), "{next:?}");

    // A re-assert that cannot run poisons the session too.
    let (c, _) = analyzer(true);
    let c = c.with_reassert_fault(true);
    let (_g, mut t) = loaded("check_flat_reassert_cannot_run", &c);
    let checked = c.reference_check(&mut t, &[lib_edit("\npub fn h() {}\n")], Scope::Affected);
    assert!(
        matches!(&checked, Err(CheckError::Poisoned(m)) if m.contains("could not run")),
        "{checked:?}"
    );
    assert_eq!(c.text(&t, &lib()).as_deref(), Some(UNRESOLVED_LIB));

    // Without `assert_revert` there is no re-assert: the write-back's texts decide.
    let (loader, _) = analyzer(false);
    let (plain, plain_obs) = analyzer(false);
    let plain = plain.with_detector_fault(Some((GapDetector::UnresolvedPath, lib())));
    // Loaded healthy, so the detector's panic is new at the check, not one of the baseline.
    let (_h, mut u) = loaded("check_flat_no_reassert", &loader);
    plain_obs.take();
    let checked = plain.reference_check(&mut u, &[lib_edit("\npub fn h() {}\n")], Scope::Affected);
    assert!(
        matches!(checked, Err(CheckError::DetectorPanicked { .. })),
        "{checked:?}"
    );
    assert_eq!(revert_oks(&plain_obs.take()), vec![None]);
    plain
        .reference_check(&mut u, &[], Scope::Affected)
        .expect("the session stays usable");
}

/// An analyzer whose `assert_revert_every` is `n`.
fn analyzer_every(n: u32) -> (RaAnalyzer, Arc<RecordingObserver>) {
    let observer = Arc::new(RecordingObserver::default());
    let a = RaAnalyzer::new(
        RaConfig {
            proc_macro: false,
            threads: Some(1),
            assert_revert: false,
            gap_detectors: true,
            assert_revert_every: std::num::NonZeroU32::new(n),
            cfg_test: false,
            ..Default::default()
        },
        Arc::new(MonotonicClock::new()),
        observer.clone(),
    );
    (a, observer)
}

#[test]
fn writes_count_for_the_sampled_isolation() {
    let (a, obs) = analyzer_every(2);
    let (_f, mut s) = loaded("check_every", &a);
    let edit = lib_edit("\npub fn h() {}\n");
    obs.take();
    // Every second check is sampled: its release checks the isolation of the base.
    for (n, want) in [(1, None), (2, Some(true)), (3, None), (4, Some(true))] {
        a.check(&mut s, std::slice::from_ref(&edit), Scope::Affected)
            .unwrap_or_else(|e| panic!("write {n}: {e:?}"));
        assert_eq!(revert_oks(&obs.take()), vec![want], "write {n}");
    }
}

#[test]
fn check_writes_are_audited() {
    let (a, _) = analyzer(false);
    let (faulty, _) = analyzer(false);
    let faulty = faulty.with_detector_fault(Some((GapDetector::UnresolvedPath, lib())));
    let (_f, mut s) = loaded("check_audit", &a);
    // The control: nothing was written, so there is nothing to audit.
    assert_eq!(faulty.audit_revert(&mut s), Ok(()));

    // A refused check is recorded too.
    let never = rel("src/never.rs");
    let refused = a.check(
        &mut s,
        &[lib_edit("\npub fn h() {}\n"), Edit::delete(never.clone())],
        Scope::Affected,
    );
    assert!(
        matches!(refused, Err(CheckError::Edit { .. })),
        "{refused:?}"
    );
    assert_eq!(s.pending_audit(), vec![lib(), never.clone()]);
    a.check(&mut s, &[lib_edit("\npub fn i() {}\n")], Scope::Affected)
        .expect("a check");
    assert_eq!(s.pending_audit(), vec![lib(), never]);

    // The audit re-diagnoses what the checks wrote: the faulty detector differs from the
    // baseline there.
    let audited = faulty.audit_revert(&mut s);
    assert!(
        matches!(audited, Err(CheckError::RevertMismatch { .. })),
        "{audited:?}"
    );
}

#[test]
fn a_poisoned_session_is_poisoned_on_the_analyzer_port() {
    let (a, _) = analyzer(false);
    let (_f, mut s) = loaded("check_poisoned_port", &a);
    let mut fork = Analyzer::fork(&a, &mut s).expect("fork");
    s.poison("test");
    let edit = lib_edit("\npub fn h() {}\n");

    // A fork already live: the operations and the discard say the poison.
    let applied = Analyzer::apply(&a, &mut s, &mut fork, std::slice::from_ref(&edit));
    assert!(
        matches!(applied, Err(AnalyzerError::Poisoned(ref m)) if m.contains("test")),
        "{applied:?}"
    );
    let diagnosed = Analyzer::diagnostics(&a, &s, &fork, Scope::Affected);
    assert!(
        matches!(diagnosed, Err(AnalyzerError::Poisoned(_))),
        "{diagnosed:?}"
    );
    let discarded = Analyzer::discard(&a, &mut s, fork);
    assert!(
        matches!(discarded, Err(AnalyzerError::Poisoned(_))),
        "{discarded:?}"
    );

    // No fork: a new one is refused with the poison.
    let forked = Analyzer::fork(&a, &mut s);
    assert!(
        matches!(forked, Err(AnalyzerError::Poisoned(_))),
        "{forked:?}"
    );
    // `verify` gives the poison as it is.
    let verified = ryo_v2_core::app::verify::verify(&a, &mut s, &[edit], Scope::Affected);
    assert!(
        matches!(verified, Err(AnalyzerError::Poisoned(_))),
        "{verified:?}"
    );
    // `expand` reports it as the session's, not as a failure of one Blueprint.
    let target = [Target::new(lib(), None)];
    for state in [Vec::new(), vec![lib_edit("\npub fn h() {}\n")]] {
        let expanded = a.expand(&mut s, &state, &target, 10);
        match expanded {
            Err(ExpandError::Session(e @ CheckError::Poisoned(_))) => {
                assert_eq!(e.reach(), CheckReach::Session);
            }
            other => panic!("expected the session's poison, got {other:?}"),
        }
    }
    assert_eq!(a.text(&s, &lib()).as_deref(), Some(UNRESOLVED_LIB));
}

#[test]
fn level_stats_has_a_row_per_engine_level() {
    let (a, _) = analyzer(false);
    let (_f, mut s) = loaded("check_level_stats", &a);
    let mut fork = Analyzer::fork(&a, &mut s).expect("fork");
    assert!(s.level_stats().is_empty(), "no level yet");
    Analyzer::apply(&a, &mut s, &mut fork, &[lib_edit("\npub fn h() {}\n")]).expect("apply");
    Analyzer::diagnostics(&a, &s, &fork, Scope::Affected).expect("diagnostics");
    let rows = s.level_stats();
    // The one level holds its diagnosis.
    assert_eq!(rows.len(), 1);
    assert!(rows[0].memos > 0, "{rows:?}");
    Analyzer::discard(&a, &mut s, fork).expect("discard");
    assert!(s.level_stats().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Fixture W: three members. `a` is a lib with a module `x`, `b` depends on `a`, `c` stands alone.
// Each crate has an E0599 and a name only the unresolved-path detector reports.
// ---------------------------------------------------------------------------------------------

const A_LIB: &str = "pub mod x;\npub struct S;\npub fn ga(s: S) {\n    s.missing();\n}\npub fn fa(_x: MissingA) {}\n";
const A_X: &str = "pub fn xf() -> u32 {\n    1\n}\n";
const B_LIB: &str = "pub mod y;\npub struct T;\npub fn gb(t: T) {\n    t.missing();\n}\npub fn fb(_x: MissingB) {}\n";
const B_Y: &str = "pub fn yf() -> u32 {\n    2\n}\n";
const C_LIB: &str = "pub mod z;\npub struct U;\npub fn gc(u: U) {\n    u.missing();\n}\npub fn fc(_x: MissingC) {}\n";
const C_Z: &str = "pub fn zf() -> u32 {\n    3\n}\n";

/// Every `.rs` file of W with its text at load.
const W_FILES: [(&str, &str); 6] = [
    ("a/src/lib.rs", A_LIB),
    ("a/src/x.rs", A_X),
    ("b/src/lib.rs", B_LIB),
    ("b/src/y.rs", B_Y),
    ("c/src/lib.rs", C_LIB),
    ("c/src/z.rs", C_Z),
];

fn base_of(path: &str) -> &'static str {
    W_FILES
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, text)| *text)
        .expect("a file of W")
}

fn w_fixture(test: &str) -> Fixture {
    let manifest = |name: &str, deps: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{deps}")
    };
    let a_cargo = manifest("wa", "");
    let b_cargo = manifest("wb", "\n[dependencies]\nwa = { path = \"../a\" }\n");
    let c_cargo = manifest("wc", "");
    let mut files: Vec<(&str, &str)> = vec![
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"2\"\n",
        ),
        ("a/Cargo.toml", &a_cargo),
        ("b/Cargo.toml", &b_cargo),
        ("c/Cargo.toml", &c_cargo),
    ];
    files.extend(W_FILES);
    Fixture::new(test, &files)
}

fn loaded_w(test: &str, a: &RaAnalyzer) -> (Fixture, RaSession) {
    let f = w_fixture(test);
    let s = Analyzer::load(a, &f.dir).expect("load W");
    (f, s)
}

/// The whole text of `path` of W with `extra` appended.
fn w_edit(path: &str, extra: &str) -> Edit {
    Edit::write(rel(path), format!("{}{extra}", base_of(path)))
}

/// A function with an error of its own.
fn err_fn(tag: &str) -> String {
    format!("pub fn e_{tag}() {{\n    1u32.nope_{tag}();\n}}\n")
}

/// Edits of W that modify, create and delete files across the crates.
fn w_mixed() -> Vec<Edit> {
    vec![
        w_edit("a/src/lib.rs", &format!("pub mod n;\n{}", err_fn("m1"))),
        Edit::write(rel("a/src/n.rs"), err_fn("new")),
        Edit::delete(rel("c/src/z.rs")),
        w_edit("b/src/y.rs", &err_fn("m2")),
    ]
}

#[test]
fn session_operations_refuse_while_an_analyzer_fork_is_live() {
    let target = [Target::new(rel("a/src/lib.rs"), None)];
    let edit = w_edit("a/src/lib.rs", &err_fn("c"));

    // A live `Analyzer` fork, whose write is not counted for the sampling (every 2nd write).
    let (a, obs) = analyzer_every(2);
    let (_f, mut s) = loaded_w("check_live_analyzer", &a);
    let mut fork = Analyzer::fork(&a, &mut s).expect("fork");
    Analyzer::apply(
        &a,
        &mut s,
        &mut fork,
        &[w_edit("c/src/lib.rs", &err_fn("l1"))],
    )
    .expect("apply");
    assert_eq!(
        a.check(&mut s, std::slice::from_ref(&edit), Scope::Affected),
        Err(CheckError::ForkInUse)
    );
    assert_eq!(a.audit_revert(&mut s), Err(CheckError::ForkInUse));
    for state in [Vec::new(), vec![edit.clone()]] {
        let expanded = a.expand(&mut s, &state, &target, 10);
        assert!(
            matches!(expanded, Err(ExpandError::Session(CheckError::ForkInUse))),
            "{expanded:?}"
        );
    }
    assert!(matches!(
        Analyzer::fork(&a, &mut s),
        Err(AnalyzerError::ForkInUse)
    ));
    Analyzer::discard(&a, &mut s, fork).expect("discard");
    // Nothing refused, the apply included, was counted: the second check is write 2.
    obs.take();
    a.check(&mut s, std::slice::from_ref(&edit), Scope::Affected)
        .expect("write 1");
    a.check(&mut s, std::slice::from_ref(&edit), Scope::Affected)
        .expect("write 2");
    assert_eq!(revert_oks(&obs.take()), vec![None, Some(true)]);
}

#[test]
fn one_level_matches_the_analyzer_path() {
    let (a, _) = analyzer(false);
    let (_f, mut s) = loaded_w("check_analyzer_path", &a);
    let edits = vec![
        w_edit("a/src/lib.rs", &err_fn("l1")),
        Edit::write(rel("a/src/n.rs"), "pub fn n() {\n    1u32.nope_n();\n}\n"),
        Edit::delete(rel("a/src/x.rs")),
    ];
    let texts = |a: &RaAnalyzer, s: &RaSession| -> Vec<Option<String>> {
        ["a/src/lib.rs", "a/src/x.rs", "a/src/n.rs"]
            .iter()
            .map(|p| a.text(s, &rel(p)))
            .collect()
    };

    for scope in [Scope::Affected, Scope::Workspace] {
        let mut lower = Analyzer::fork(&a, &mut s).expect("fork");
        Analyzer::apply(&a, &mut s, &mut lower, &edits).expect("apply");
        let via_analyzer = Analyzer::diagnostics(&a, &s, &lower, scope).expect("diagnostics");
        Analyzer::discard(&a, &mut s, lower).expect("discard");
        let after_analyzer = texts(&a, &s);

        let via_check = a.check(&mut s, &edits, scope).expect("check");
        let after_check = texts(&a, &s);

        assert_eq!(via_analyzer.files, via_check.files, "{scope:?}");
        assert_eq!(via_analyzer.findings, via_check.findings, "{scope:?}");
        assert_eq!(after_analyzer, after_check, "{scope:?}");
        assert_eq!(
            after_check,
            vec![Some(A_LIB.to_owned()), Some(A_X.to_owned()), None]
        );
    }
}

#[test]
fn reference_check_runs_no_layer_code() {
    let (a, _) = analyzer(false);
    let (_f, mut s) = loaded_w("check_reference_check", &a);
    let edits = w_mixed();
    let counters = s.fork_counters().expect("no fork is live");
    assert!(!counters.frozen);

    let reference = a
        .reference_check(&mut s, &edits, Scope::Affected)
        .expect("the reference check");
    // No layer existed during it: nothing a layer counts moved, and the base is not frozen.
    assert_eq!(s.fork_counters(), Some(counters.clone()));
    assert!(!reference.findings.is_empty(), "the edits add errors");
    // The session is at its base.
    for (path, text) in W_FILES {
        assert_eq!(a.text(&s, &rel(path)).as_deref(), Some(text), "{path}");
    }
    // Its result is the check's, on a layer.
    let checked = a.check(&mut s, &edits, Scope::Affected).expect("the check");
    assert_eq!(checked, reference);
    // And again after the check, which leaves the reference as it was.
    let again = a
        .reference_check(&mut s, &edits, Scope::Affected)
        .expect("the reference check again");
    assert_eq!(again, reference);
    assert_eq!(s.fork_counters().map(|c| c.refused), Some(0));
}
