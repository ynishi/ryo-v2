//! The cut-off of `Verifier::check` and the revert audit, on a throwaway workspace.

mod common;

use common::{Fixture, rel};
use ryo_v2_core::app::ports::{Analyzer, CheckError, RevertAudit, Verifier};
use ryo_v2_core::domain::{Edit, Scope};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_ra::{RaAnalyzer, RaConfig, RaSession};
use std::sync::Arc;

const LIB: &str = "pub struct S;\n\npub fn a(s: &S) -> u32 {\n    s.missing()\n}\n";

fn analyzer(timeout_ms: Option<u64>) -> RaAnalyzer {
    RaAnalyzer::new(
        RaConfig {
            proc_macro: false,
            threads: Some(1),
            assert_revert: false,
            gap_detectors: true,
            assert_revert_every: None,
            cfg_test: false,
            engine: common::engine(),
        },
        Arc::new(MonotonicClock::new()),
        Arc::new(RecordingObserver::default()),
    )
    .with_check_timeout(timeout_ms)
}

fn lib_edit(extra: &str) -> Edit {
    Edit::write(rel("src/lib.rs"), format!("{LIB}{extra}"))
}

fn loaded(test: &str) -> (Fixture, RaAnalyzer, RaSession) {
    let f = Fixture::single(test, LIB);
    let a = analyzer(None);
    let s = a.load(&f.dir).expect("load fixture");
    (f, a, s)
}

#[test]
fn check_past_timeout_is_timed_out_and_session_usable() {
    common::both(check_past_timeout_is_timed_out_and_session_usable_on);
}

fn check_past_timeout_is_timed_out_and_session_usable_on() {
    let (_f, plain, mut s) = loaded("timeout_cut");
    let base_findings = Verifier::baseline(&plain, &s);
    let edit = lib_edit("\npub fn b() {}\n");

    let cut = analyzer(Some(0));
    let err = cut
        .check(&mut s, std::slice::from_ref(&edit), Scope::Affected)
        .expect_err("a 0 ms limit cuts the diagnostics off");
    assert_eq!(err, CheckError::TimedOut { limit_ms: 0 });
    assert_eq!(plain.text(&s, &rel("src/lib.rs")).as_deref(), Some(LIB));
    assert_eq!(Verifier::baseline(&plain, &s), base_findings);

    // A generous limit does not cut.
    let roomy = analyzer(Some(600_000));
    roomy
        .check(&mut s, std::slice::from_ref(&edit), Scope::Affected)
        .expect("a roomy limit passes");

    // The session is usable without a limit, and it is at base.
    plain
        .check(&mut s, &[edit], Scope::Affected)
        .expect("check after a timeout");
    assert_eq!(plain.text(&s, &rel("src/lib.rs")).as_deref(), Some(LIB));
    assert_eq!(Verifier::baseline(&plain, &s), base_findings);
}

#[test]
fn audit_revert_is_ok_after_checks_and_refuses_poisoned() {
    common::both(audit_revert_is_ok_after_checks_and_refuses_poisoned_on);
}

fn audit_revert_is_ok_after_checks_and_refuses_poisoned_on() {
    let (_f, a, mut s) = loaded("timeout_audit");
    // Nothing recorded: Ok.
    a.audit_revert(&mut s).expect("nothing to audit");
    for extra in ["\npub fn b() {}\n", "\npub fn c() {}\n"] {
        a.check(&mut s, &[lib_edit(extra)], Scope::Affected)
            .expect("check");
    }
    a.audit_revert(&mut s).expect("clean reverts audit clean");
    a.audit_revert(&mut s).expect("the record was cleared");

    s.poison("test");
    assert!(matches!(
        a.audit_revert(&mut s),
        Err(CheckError::Poisoned(_))
    ));
    assert!(matches!(
        a.check(&mut s, &[lib_edit("")], Scope::Affected),
        Err(CheckError::Poisoned(_))
    ));
}

#[test]
fn a_failed_audit_keeps_its_paths() {
    common::both(a_failed_audit_keeps_its_paths_on);
}

fn a_failed_audit_keeps_its_paths_on() {
    let f = Fixture::new(
        "timeout_audit_fault",
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
            ),
            ("src/lib.rs", "pub mod a;\npub mod b;\n"),
            ("src/a.rs", "pub fn a() {}\n"),
            ("src/b.rs", "pub fn b() {}\n"),
        ],
    );
    let plain = analyzer(None);
    let mut s = plain.load(&f.dir).expect("load fixture");
    for (path, text) in [
        ("src/a.rs", "pub fn a() {}\n\npub fn a2() {}\n"),
        ("src/b.rs", "pub fn b() {}\n\npub fn b2() {}\n"),
    ] {
        plain
            .check(&mut s, &[Edit::write(rel(path), text)], Scope::Affected)
            .expect("check");
    }
    let both = vec![rel("src/a.rs"), rel("src/b.rs")];
    assert_eq!(s.pending_audit(), both);

    // An audit that cannot run returns the failure and keeps what it was to audit.
    let faulty = analyzer(None).with_audit_fault(true);
    assert!(matches!(
        faulty.audit_revert(&mut s),
        Err(CheckError::Panicked(_))
    ));
    assert_eq!(s.pending_audit(), both);

    // The next audit diagnoses them and forgets them.
    plain
        .audit_revert(&mut s)
        .expect("clean reverts audit clean");
    assert!(s.pending_audit().is_empty());
}
