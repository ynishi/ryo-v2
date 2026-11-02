//! The levels of a fork, and the checks and discards built on them, on throwaway workspaces (the
//! corpus is never touched).

mod common;

use common::{Fixture, rel};
use ryo_v2_core::app::ports::{
    Analyzer, AnalyzerError, CheckError, CheckReach, Event, ExpandError, Expander, RevertAudit,
    Stacked, Verifier,
};
use ryo_v2_core::domain::baseline::{new_findings, restrict};
use ryo_v2_core::domain::{Edit, RelPath, Scope, ScopedFindings, Target, UnseenClass, Verdict};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_ra::{DbWritePanic, GapDetector, RaAnalyzer, RaConfig, RaSession, RaStack};
use std::collections::{BTreeMap, BTreeSet};
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
        },
        Arc::new(MonotonicClock::new()),
        observer.clone(),
    );
    (a, observer)
}

/// The whole text of `path` of W with `extra` appended.
fn w_edit(path: &str, extra: &str) -> Edit {
    Edit::write(rel(path), format!("{}{extra}", base_of(path)))
}

/// A function with an error of its own.
fn err_fn(tag: &str) -> String {
    format!("pub fn e_{tag}() {{\n    1u32.nope_{tag}();\n}}\n")
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

fn checked_count(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, Event::Checked { .. }))
        .count()
}

fn w_paths() -> Vec<RelPath> {
    W_FILES.iter().map(|(p, _)| rel(p)).collect()
}

/// What a test compares from one level of a fork: the number of levels, the texts of `paths` and
/// the findings of the whole workspace.
#[derive(Debug, PartialEq)]
struct Snap {
    levels: usize,
    texts: Vec<Option<String>>,
    found: ScopedFindings,
}

fn snap(a: &RaAnalyzer, s: &RaSession, f: &RaStack, paths: &[RelPath]) -> Snap {
    Snap {
        levels: a.levels(s, f).expect("levels"),
        texts: paths
            .iter()
            .map(|p| a.fork_text(s, f, p).expect("fork_text"))
            .collect(),
        found: a.diagnose(s, f, Scope::Workspace).expect("diagnose"),
    }
}

#[test]
fn levels_stack_and_pop_back_to_the_level_below() {
    let (a, _) = analyzer(false);
    let (_f, mut s) = loaded_w("stack_levels", &a);
    let paths = [rel("c/src/lib.rs"), rel("a/src/lib.rs")];
    let a2 = format!("{A_LIB}{}", err_fn("l2"));
    let a3 = format!("{a2}{}", err_fn("l3"));
    let levels = [
        vec![w_edit("c/src/lib.rs", &err_fn("l1"))],
        vec![Edit::write(rel("a/src/lib.rs"), a2.clone())],
        vec![Edit::write(rel("a/src/lib.rs"), a3.clone())],
    ];

    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    let mut snaps = vec![snap(&a, &s, &fork, &paths)];
    assert_eq!(snaps[0].levels, 0);
    assert_eq!(snaps[0].found, ScopedFindings::default());
    assert_eq!(
        snaps[0].texts,
        vec![Some(C_LIB.to_owned()), Some(A_LIB.to_owned())]
    );
    for (i, edits) in levels.iter().enumerate() {
        a.write(&mut s, &mut fork, edits).expect("write");
        snaps.push(snap(&a, &s, &fork, &paths));
        assert_eq!(snaps[i + 1].levels, i + 1);
    }
    assert_eq!(snaps[3].texts[1].as_deref(), Some(a3.as_str()));
    assert_eq!(snaps[2].texts[1].as_deref(), Some(a2.as_str()));
    assert_eq!(snaps[1].texts[1].as_deref(), Some(A_LIB));
    let counts: Vec<usize> = snaps.iter().map(|x| x.found.findings.len()).collect();
    assert!(
        counts[1] > 0 && counts[1] < counts[2] && counts[2] < counts[3],
        "each level adds an error: {counts:?}"
    );

    for i in (0..3).rev() {
        assert!(a.pop(&mut s, &mut fork).expect("pop"));
        assert_eq!(
            snap(&a, &s, &fork, &paths),
            snaps[i],
            "after the pop to level {i}"
        );
    }
    a.release(&mut s, fork).expect("release");
    assert_eq!(a.text(&s, &rel("a/src/lib.rs")).as_deref(), Some(A_LIB));
}

/// The edits that take the base of W to the state after `levels`: per path the last write wins; a
/// path created and then deleted gives no edit; a path deleted and then written gives a write.
fn compose(levels: &[Vec<Edit>]) -> Vec<Edit> {
    let mut net: BTreeMap<RelPath, Option<String>> = BTreeMap::new();
    for edit in levels.iter().flatten() {
        net.insert(edit.path.clone(), edit.new_text.clone());
    }
    net.into_iter()
        .filter_map(|(path, text)| match text {
            Some(text) => Some(Edit::write(path, text)),
            None => W_FILES
                .iter()
                .any(|(p, _)| *p == path.as_str())
                .then(|| Edit::delete(path)),
        })
        .collect()
}

/// Five sequences of three levels over W.
fn sequences() -> Vec<Vec<Vec<Edit>>> {
    let a_lib_with_m = format!("{A_LIB}pub mod m;\n");
    let m1 = "pub fn mf() -> u32 {\n    1\n}\n".to_owned();
    let m2 = format!("{m1}{}", err_fn("m"));
    let a_lib_no_x = A_LIB.replace("pub mod x;\n", "");
    vec![
        // Modify only: three crates in turn.
        vec![
            vec![w_edit("c/src/lib.rs", &err_fn("s1c"))],
            vec![w_edit("a/src/x.rs", &err_fn("s1a"))],
            vec![w_edit("b/src/y.rs", &err_fn("s1b"))],
        ],
        // Create (with its `mod`), modify the created file, overwrite an earlier level's file.
        vec![
            vec![
                Edit::write(rel("a/src/lib.rs"), a_lib_with_m.clone()),
                Edit::write(rel("a/src/m.rs"), m1.clone()),
            ],
            vec![Edit::write(rel("a/src/m.rs"), m2.clone())],
            vec![Edit::write(
                rel("a/src/lib.rs"),
                format!("{a_lib_with_m}{}", err_fn("s2")),
            )],
        ],
        // Delete a file and its `mod`, edit elsewhere, edit the crate again.
        vec![
            vec![
                Edit::write(rel("a/src/lib.rs"), a_lib_no_x.clone()),
                Edit::delete(rel("a/src/x.rs")),
            ],
            vec![w_edit("b/src/lib.rs", &err_fn("s3b"))],
            vec![Edit::write(
                rel("a/src/lib.rs"),
                format!("{a_lib_no_x}{}", err_fn("s3a")),
            )],
        ],
        // Delete, then revive in the level above.
        vec![
            vec![
                Edit::write(rel("a/src/lib.rs"), a_lib_no_x),
                Edit::delete(rel("a/src/x.rs")),
            ],
            vec![w_edit("c/src/z.rs", &err_fn("s4c"))],
            vec![
                Edit::write(rel("a/src/x.rs"), format!("{A_X}{}", err_fn("s4x"))),
                Edit::write(rel("a/src/lib.rs"), A_LIB),
            ],
        ],
        // Create, then delete in the level above.
        vec![
            vec![
                Edit::write(rel("a/src/lib.rs"), a_lib_with_m),
                Edit::write(rel("a/src/m.rs"), m2),
            ],
            vec![
                Edit::write(rel("a/src/lib.rs"), A_LIB),
                Edit::delete(rel("a/src/m.rs")),
            ],
            vec![w_edit("b/src/lib.rs", &err_fn("s5"))],
        ],
    ]
}

#[test]
fn a_stack_diagnoses_as_its_flat_composition() {
    let (a, _) = analyzer(false);
    let f = w_fixture("stack_flat");
    let mut s = Analyzer::load(&a, &f.dir).expect("load the stacked session");
    let mut twin = Analyzer::load(&a, &f.dir).expect("load the flat twin");

    for (n, levels) in sequences().iter().enumerate() {
        let mut fork = Stacked::fork(&a, &mut s).expect("fork");
        for (k, edits) in levels.iter().enumerate() {
            let at = format!("sequence {n}, level {}", k + 1);
            a.write(&mut s, &mut fork, edits)
                .unwrap_or_else(|e| panic!("{at}: {e:?}"));
            let flat_edits = compose(&levels[..=k]);
            let flat = a.check(&mut twin, &flat_edits, Scope::Workspace);
            let stacked = a.diagnose(&s, &fork, Scope::Workspace);
            assert_eq!(stacked, flat, "{at}: Workspace");
            let flat = flat.expect("the flat check");
            assert_eq!(
                stacked.as_ref().map(|d| &d.unseen_at),
                Ok(&flat.unseen_at),
                "{at}: unseen_at"
            );

            let affected = a.diagnose(&s, &fork, Scope::Affected).expect("affected");
            assert!(
                affected.files.iter().all(|p| flat.files.contains(p)),
                "{at}: the affected files are files of the state"
            );
            assert_eq!(
                affected.findings,
                restrict(&flat.findings, &affected.files),
                "{at}: Affected"
            );
        }
        a.release(&mut s, fork).expect("release");
    }
}

#[test]
fn a_refused_write_pushes_no_level() {
    let (a, _) = analyzer(false);
    let (_f, mut s) = loaded_w("stack_refused_write", &a);
    let c_text = format!("{C_LIB}{}", err_fn("l1"));
    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    a.write(
        &mut s,
        &mut fork,
        &[Edit::write(rel("c/src/lib.rs"), c_text.clone())],
    )
    .expect("level 1");
    let ok_rs = rel("a/src/ok.rs");
    let unchanged = |a: &RaAnalyzer, s: &RaSession, fork: &RaStack, what: &str| {
        assert_eq!(a.levels(s, fork), Ok(1), "{what}");
        assert_eq!(a.fork_text(s, fork, &ok_rs), Ok(None), "{what}");
        assert_eq!(
            a.fork_text(s, fork, &rel("c/src/lib.rs")),
            Ok(Some(c_text.clone())),
            "{what}"
        );
        assert_eq!(
            a.fork_text(s, fork, &rel("a/src/lib.rs")),
            Ok(Some(A_LIB.to_owned())),
            "{what}"
        );
        let found = a.diagnose(s, fork, Scope::Workspace).expect(what);
        assert!(!found.files.contains(&ok_rs), "{what}: {:?}", found.files);
    };

    // Refused while resolving.
    let refused = a.write(
        &mut s,
        &mut fork,
        &[Edit::write(rel("Cargo.toml"), "[workspace]\n")],
    );
    assert!(
        matches!(refused, Err(CheckError::Edit { .. })),
        "{refused:?}"
    );
    unchanged(&a, &s, &fork, "after the manifest");

    // A write of several edits, the last refused: the earlier ones leave nothing either.
    let refused = a.write(
        &mut s,
        &mut fork,
        &[
            Edit::write(ok_rs.clone(), "pub fn ok() {}\n"),
            Edit::write(rel("a/src/lib.rs"), format!("{A_LIB}pub mod ok;\n")),
            Edit::delete(rel("a/src/never.rs")),
        ],
    );
    assert!(
        matches!(refused, Err(CheckError::Edit { .. })),
        "{refused:?}"
    );
    unchanged(&a, &s, &fork, "after the file never known");

    // The next write works, and `a/src/ok.rs` can be created.
    a.write(
        &mut s,
        &mut fork,
        &[Edit::write(ok_rs.clone(), "pub fn ok() {}\n")],
    )
    .expect("the next write");
    assert_eq!(a.levels(&s, &fork), Ok(2));
    assert_eq!(
        a.fork_text(&s, &fork, &ok_rs),
        Ok(Some("pub fn ok() {}\n".to_owned()))
    );
    assert!(a.pop(&mut s, &mut fork).expect("pop"));
    assert_eq!(a.fork_text(&s, &fork, &ok_rs), Ok(None));
    a.release(&mut s, fork).expect("release");
}

#[test]
fn a_level_writes_against_the_state_below() {
    let (a, _) = analyzer(false);
    let (_f, mut s) = loaded_w("stack_resolution", &a);
    let m = rel("a/src/m.rs");
    let lib_m = format!("{A_LIB}pub mod m;\n");
    let text = |a: &RaAnalyzer, s: &RaSession, f: &RaStack, p: &RelPath| {
        a.fork_text(s, f, p).expect("fork_text")
    };

    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    // L1 creates `a/src/m.rs` (with its `mod`).
    a.write(
        &mut s,
        &mut fork,
        &[
            Edit::write(rel("a/src/lib.rs"), lib_m.clone()),
            Edit::write(m.clone(), "pub fn m1() {}\n"),
        ],
    )
    .expect("L1");
    assert_eq!(text(&a, &s, &fork, &m).as_deref(), Some("pub fn m1() {}\n"));
    // L2 modifies the file L1 created.
    a.write(
        &mut s,
        &mut fork,
        &[Edit::write(m.clone(), "pub fn m2() {}\n")],
    )
    .expect("L2");
    assert_eq!(text(&a, &s, &fork, &m).as_deref(), Some("pub fn m2() {}\n"));
    // L3 deletes it, and a load-time file with it.
    a.write(
        &mut s,
        &mut fork,
        &[Edit::delete(m.clone()), Edit::delete(rel("a/src/x.rs"))],
    )
    .expect("L3");
    assert_eq!(text(&a, &s, &fork, &m), None);
    assert_eq!(text(&a, &s, &fork, &rel("a/src/x.rs")), None);
    // Deleting a path that is not a file of the state below is refused, a never-known path too.
    for gone in [m.clone(), rel("a/src/x.rs"), rel("a/src/never.rs")] {
        let refused = a.write(&mut s, &mut fork, &[Edit::delete(gone)]);
        assert!(
            matches!(refused, Err(CheckError::Edit { .. })),
            "{refused:?}"
        );
        assert_eq!(a.levels(&s, &fork), Ok(3));
    }
    // L4 writes both again: a path a level below deleted is revived, not refused.
    a.write(
        &mut s,
        &mut fork,
        &[
            Edit::write(m.clone(), "pub fn m4() {}\n"),
            Edit::write(rel("a/src/x.rs"), "pub fn x4() {}\n"),
        ],
    )
    .expect("L4");
    assert_eq!(text(&a, &s, &fork, &m).as_deref(), Some("pub fn m4() {}\n"));
    assert_eq!(
        text(&a, &s, &fork, &rel("a/src/x.rs")).as_deref(),
        Some("pub fn x4() {}\n")
    );
    let found = a.diagnose(&s, &fork, Scope::Workspace).expect("diagnose");
    assert!(found.files.contains(&m) && found.files.contains(&rel("a/src/x.rs")));
    // L5 deletes the revived file once more.
    a.write(&mut s, &mut fork, &[Edit::delete(m.clone())])
        .expect("L5");
    assert_eq!(text(&a, &s, &fork, &m), None);

    // Each pop restores the state below.
    assert!(a.pop(&mut s, &mut fork).expect("pop L5"));
    assert_eq!(text(&a, &s, &fork, &m).as_deref(), Some("pub fn m4() {}\n"));
    assert!(a.pop(&mut s, &mut fork).expect("pop L4"));
    assert_eq!(text(&a, &s, &fork, &m), None);
    assert_eq!(text(&a, &s, &fork, &rel("a/src/x.rs")), None);
    assert!(a.pop(&mut s, &mut fork).expect("pop L3"));
    assert_eq!(text(&a, &s, &fork, &m).as_deref(), Some("pub fn m2() {}\n"));
    assert_eq!(
        text(&a, &s, &fork, &rel("a/src/x.rs")).as_deref(),
        Some(A_X)
    );
    assert!(a.pop(&mut s, &mut fork).expect("pop L2"));
    assert_eq!(text(&a, &s, &fork, &m).as_deref(), Some("pub fn m1() {}\n"));
    assert!(a.pop(&mut s, &mut fork).expect("pop L1"));
    assert_eq!(text(&a, &s, &fork, &m), None);
    assert_eq!(
        text(&a, &s, &fork, &rel("a/src/lib.rs")).as_deref(),
        Some(A_LIB)
    );
    a.release(&mut s, fork).expect("release");
    assert_eq!(a.text(&s, &m), None);
}

#[test]
fn affected_scope_is_the_top_levels() {
    let (a, _) = analyzer(false);
    let (_f, mut s) = loaded_w("stack_affected", &a);
    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    a.write(
        &mut s,
        &mut fork,
        &[
            w_edit("c/src/lib.rs", &err_fn("l1")),
            Edit::write(rel("c/src/extra.rs"), "pub fn extra() {}\n"),
        ],
    )
    .expect("L1");
    let at_l1 = a.diagnose(&s, &fork, Scope::Affected).expect("L1 affected");
    assert_eq!(
        at_l1.files,
        vec![
            rel("c/src/extra.rs"),
            rel("c/src/lib.rs"),
            rel("c/src/z.rs")
        ]
    );

    a.write(
        &mut s,
        &mut fork,
        &[
            w_edit("a/src/lib.rs", &err_fn("l2")),
            Edit::write(rel("a/src/n.rs"), "pub fn n() {}\n"),
            Edit::delete(rel("a/src/x.rs")),
        ],
    )
    .expect("L2");
    let at_l2 = a.diagnose(&s, &fork, Scope::Affected).expect("L2 affected");
    // a's live files and b's: the created one in, the deleted one out, none of c.
    assert_eq!(
        at_l2.files,
        vec![
            rel("a/src/lib.rs"),
            rel("a/src/n.rs"),
            rel("b/src/lib.rs"),
            rel("b/src/y.rs")
        ]
    );
    let workspace = a.diagnose(&s, &fork, Scope::Workspace).expect("workspace");
    assert_eq!(
        workspace.files,
        vec![
            rel("a/src/lib.rs"),
            rel("a/src/n.rs"),
            rel("b/src/lib.rs"),
            rel("b/src/y.rs"),
            rel("c/src/extra.rs"),
            rel("c/src/lib.rs"),
            rel("c/src/z.rs")
        ]
    );
    a.release(&mut s, fork).expect("release");
}

#[test]
fn files_outside_the_top_scope_keep_the_level_below() {
    let (a, _) = analyzer(false);
    let (_f, mut s) = loaded_w("stack_outside_scope", &a);
    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    // L1 also leaves a macro call in a file of a unexpanded, so a class is observed there.
    a.write(
        &mut s,
        &mut fork,
        &[
            w_edit("a/src/lib.rs", &err_fn("l1")),
            w_edit("a/src/x.rs", "not_a_macro!();\n"),
        ],
    )
    .expect("L1");
    let at_l1 = a.diagnose(&s, &fork, Scope::Workspace).expect("L1");
    a.write(&mut s, &mut fork, &[w_edit("c/src/lib.rs", &err_fn("l2"))])
        .expect("L2");
    let at_l2 = a.diagnose(&s, &fork, Scope::Workspace).expect("L2");

    let outside: Vec<RelPath> = ["a/src/lib.rs", "a/src/x.rs", "b/src/lib.rs", "b/src/y.rs"]
        .iter()
        .map(|p| rel(p))
        .collect();
    let kept = restrict(&at_l1.findings, &outside);
    assert!(
        kept.len() > restrict(&Verifier::baseline(&a, &s), &outside).len(),
        "level 1 added an error to a: {kept:?}"
    );
    assert_eq!(restrict(&at_l2.findings, &outside), kept);
    assert!(
        at_l2.findings.len() > at_l1.findings.len(),
        "level 2 added one in c"
    );
    // The classes of a's files stay as well: the same files under each class.
    let classes_of = |found: &ScopedFindings| -> BTreeMap<UnseenClass, BTreeSet<RelPath>> {
        found
            .unseen_at
            .iter()
            .map(|(class, on)| {
                let kept: BTreeSet<RelPath> =
                    on.iter().filter(|p| outside.contains(p)).cloned().collect();
                (*class, kept)
            })
            .filter(|(_, on)| !on.is_empty())
            .collect()
    };
    assert_eq!(
        classes_of(&at_l1),
        BTreeMap::from([(
            UnseenClass::UnexpandedMacro,
            BTreeSet::from([rel("a/src/x.rs")])
        )])
    );
    assert_eq!(classes_of(&at_l2), classes_of(&at_l1));
    a.release(&mut s, fork).expect("release");
}

#[test]
fn an_empty_write_diagnoses_nothing() {
    let (a, _) = analyzer(false);
    let (_f, mut s) = loaded_w("stack_empty_write", &a);
    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    a.write(&mut s, &mut fork, &[]).expect("an empty write");
    assert_eq!(a.levels(&s, &fork), Ok(1));
    let affected = a.diagnose(&s, &fork, Scope::Affected).expect("affected");
    assert!(affected.files.is_empty(), "{:?}", affected.files);
    assert!(affected.unseen().is_empty(), "{:?}", affected.unseen());
    assert!(affected.findings.is_empty());
    let workspace = a.diagnose(&s, &fork, Scope::Workspace).expect("workspace");
    assert_eq!(workspace.files, w_paths_sorted());
    assert!(a.pop(&mut s, &mut fork).expect("pop"));
    assert_eq!(a.levels(&s, &fork), Ok(0));
    a.release(&mut s, fork).expect("release");
}

fn w_paths_sorted() -> Vec<RelPath> {
    let mut paths = w_paths();
    paths.sort();
    paths
}

#[test]
fn a_cut_off_diagnosis_keeps_the_levels() {
    let (a, obs) = analyzer(false);
    let cut = a.with_check_timeout(Some(0));
    let (roomy, _) = analyzer(false);
    let roomy = roomy.with_check_timeout(Some(600_000));
    let (_f, mut s) = loaded_w("stack_cut_off", &cut);
    let text = format!("{A_LIB}{}", err_fn("l1"));

    let mut fork = Stacked::fork(&cut, &mut s).expect("fork");
    cut.write(
        &mut s,
        &mut fork,
        &[Edit::write(rel("a/src/lib.rs"), text.clone())],
    )
    .expect("L1");
    obs.take();
    let cut_off = cut.diagnose(&s, &fork, Scope::Workspace);
    assert_eq!(cut_off, Err(CheckError::TimedOut { limit_ms: 0 }));
    assert_eq!(
        checked_count(&obs.take()),
        0,
        "a cut-off diagnosis records no check"
    );
    assert_eq!(cut.levels(&s, &fork), Ok(1));
    assert_eq!(
        cut.fork_text(&s, &fork, &rel("a/src/lib.rs")),
        Ok(Some(text.clone()))
    );

    // The fork is usable: a roomy analyzer diagnoses the same level.
    let found = roomy
        .diagnose(&s, &fork, Scope::Workspace)
        .expect("a roomy diagnosis");
    assert_eq!(found.files, w_paths_sorted());
    assert!(cut.pop(&mut s, &mut fork).expect("pop"));
    cut.write(&mut s, &mut fork, &[])
        .expect("write after the pop");
    cut.release(&mut s, fork).expect("release");
    assert_eq!(cut.text(&s, &rel("a/src/lib.rs")).as_deref(), Some(A_LIB));
}

#[test]
fn a_level_names_the_unseen_it_observes() {
    let (a, _) = analyzer(false);
    let (_f, mut s) = loaded_w("stack_unseen", &a);
    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    a.write(&mut s, &mut fork, &[w_edit("c/src/lib.rs", &err_fn("l1"))])
        .expect("L1");
    let at_l1 = a.diagnose(&s, &fork, Scope::Affected).expect("L1");
    assert!(at_l1.unseen().is_empty(), "{:?}", at_l1.unseen());

    a.write(
        &mut s,
        &mut fork,
        &[w_edit("a/src/x.rs", "not_a_macro!();\n")],
    )
    .expect("L2");
    let at_l2 = a.diagnose(&s, &fork, Scope::Affected).expect("L2");
    assert_eq!(
        at_l2.unseen(),
        BTreeSet::from([UnseenClass::UnexpandedMacro])
    );

    // A level above that does not observe it does not name it.
    a.write(&mut s, &mut fork, &[w_edit("c/src/z.rs", &err_fn("l3"))])
        .expect("L3");
    let at_l3 = a.diagnose(&s, &fork, Scope::Affected).expect("L3");
    assert!(at_l3.unseen().is_empty(), "{:?}", at_l3.unseen());

    assert!(a.pop(&mut s, &mut fork).expect("pop L3"));
    assert!(a.pop(&mut s, &mut fork).expect("pop L2"));
    let again = a.diagnose(&s, &fork, Scope::Affected).expect("L1 again");
    assert!(again.unseen().is_empty(), "{:?}", again.unseen());
    a.release(&mut s, fork).expect("release");
}

#[test]
fn a_baseline_detector_panic_is_named_at_every_level() {
    let (a, _) = analyzer(false);
    let a = a.with_detector_fault(Some((GapDetector::UnresolvedPath, rel("a/src/x.rs"))));
    let (_f, mut s) = loaded_w("stack_baseline_panic", &a);
    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    for tag in ["l1", "l2"] {
        a.write(&mut s, &mut fork, &[w_edit("a/src/lib.rs", &err_fn(tag))])
            .expect("write");
        let found = a
            .diagnose(&s, &fork, Scope::Affected)
            .unwrap_or_else(|e| panic!("{tag}: a baseline panic fails the level: {e:?}"));
        assert!(
            found.unseen().contains(&UnseenClass::UnresolvedPath),
            "{tag}: {:?}",
            found.unseen()
        );
    }
    a.release(&mut s, fork).expect("release");
}

#[test]
fn a_new_detector_panic_fails_the_diagnose_and_keeps_the_level() {
    let (a, _) = analyzer(false);
    let a = a.with_detector_fault(Some((GapDetector::UnresolvedPath, rel("a/src/m.rs"))));
    let f = w_fixture("stack_new_panic");
    let mut s = Analyzer::load(&a, &f.dir).expect("load");
    let mut twin = Analyzer::load(&a, &f.dir).expect("load the flat twin");
    let m = rel("a/src/m.rs");
    let panicked = |r: &Result<ScopedFindings, CheckError>| {
        matches!(r, Err(CheckError::DetectorPanicked { detector, file })
            if file == &m && detector == GapDetector::UnresolvedPath.name())
    };

    let l1 = vec![Edit::write(m.clone(), "pub fn m() {}\n")];
    let l2 = vec![w_edit("a/src/lib.rs", &err_fn("l2"))];
    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    a.write(&mut s, &mut fork, &l1).expect("L1");
    let at_l1 = a.diagnose(&s, &fork, Scope::Affected);
    assert!(panicked(&at_l1), "{at_l1:?}");
    assert_eq!(a.levels(&s, &fork), Ok(1));
    assert_eq!(
        a.fork_text(&s, &fork, &m),
        Ok(Some("pub fn m() {}\n".to_owned()))
    );

    // Built on the failed level, the next one fails as the flat check does: the detector is blind
    // on that file in a state judged against the base.
    a.write(&mut s, &mut fork, &l2).expect("L2");
    let at_l2 = a.diagnose(&s, &fork, Scope::Affected);
    assert!(panicked(&at_l2), "{at_l2:?}");
    let flat = a.check(
        &mut twin,
        &compose(&[l1.clone(), l2.clone()]),
        Scope::Affected,
    );
    assert_eq!(at_l2, flat);

    assert!(a.pop(&mut s, &mut fork).expect("pop"));
    assert!(a.pop(&mut s, &mut fork).expect("pop"));
    a.release(&mut s, fork).expect("release");
    let again = Stacked::fork(&a, &mut s).expect("a new fork works");
    a.release(&mut s, again).expect("release");
    assert_eq!(a.text(&s, &m), None);
}

#[test]
fn a_baseline_panic_brought_back_above_is_named() {
    let (a, _) = analyzer(false);
    let a = a.with_detector_fault(Some((GapDetector::UnresolvedPath, rel("a/src/x.rs"))));
    let f = w_fixture("stack_panic_back");
    let mut s = Analyzer::load(&a, &f.dir).expect("load");
    let mut twin = Analyzer::load(&a, &f.dir).expect("load the flat twin");
    let l1 = vec![
        Edit::write(rel("a/src/lib.rs"), A_LIB.replace("pub mod x;\n", "")),
        Edit::delete(rel("a/src/x.rs")),
    ];
    let l2 = vec![
        Edit::write(rel("a/src/x.rs"), A_X),
        Edit::write(rel("a/src/lib.rs"), A_LIB),
    ];

    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    a.write(&mut s, &mut fork, &l1).expect("L1");
    let at_l1 = a.diagnose(&s, &fork, Scope::Affected).expect("L1");
    assert!(
        !at_l1.unseen().contains(&UnseenClass::UnresolvedPath),
        "the faulted file is not there: {:?}",
        at_l1.unseen()
    );
    a.write(&mut s, &mut fork, &l2).expect("L2");
    let at_l2 = a
        .diagnose(&s, &fork, Scope::Affected)
        .expect("a baseline panic brought back is named, not failed");
    assert!(
        at_l2.unseen().contains(&UnseenClass::UnresolvedPath),
        "{:?}",
        at_l2.unseen()
    );
    let flat = a.check(&mut twin, &compose(&[l1, l2]), Scope::Affected);
    assert_eq!(Ok(at_l2), flat);
    a.release(&mut s, fork).expect("release");
}

#[test]
fn pop_compares_texts_and_poisons_on_a_difference() {
    let (a, _) = analyzer(false);
    let a = a.with_revert_fault(Some(rel("a/src/lib.rs")));
    let (_f, mut s) = loaded_w("stack_pop_texts", &a);
    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    a.write(&mut s, &mut fork, &[w_edit("c/src/lib.rs", &err_fn("l1"))])
        .expect("L1");
    a.write(&mut s, &mut fork, &[w_edit("a/src/lib.rs", &err_fn("l2"))])
        .expect("L2");

    let popped = a.pop(&mut s, &mut fork);
    assert_eq!(
        popped,
        Err(CheckError::RevertMismatch {
            paths: vec![rel("a/src/lib.rs")]
        })
    );
    assert_eq!(a.levels(&s, &fork), Ok(1), "the level is removed");
    let released = a.release(&mut s, fork);
    assert!(
        matches!(released, Err(CheckError::Poisoned(_))),
        "{released:?}"
    );
    let next = Stacked::fork(&a, &mut s);
    assert!(matches!(next, Err(CheckError::Poisoned(_))), "{next:?}");
}

#[test]
fn a_revert_that_cannot_be_confirmed_poisons_the_session() {
    // Through a check: the texts come back, the re-assert cannot run, and the session stops.
    let (a, _) = analyzer(true);
    let a = a.with_reassert_fault(true);
    let (_f, mut s) = loaded("stack_reassert_cannot_run", &a);
    let checked = a.check(&mut s, &[lib_edit("\npub fn h() {}\n")], Scope::Affected);
    assert!(
        matches!(&checked, Err(CheckError::Poisoned(m)) if m.contains("could not run")),
        "{checked:?}"
    );
    assert_eq!(a.text(&s, &lib()).as_deref(), Some(UNRESOLVED_LIB));
    let next = a.check(&mut s, &[], Scope::Affected);
    assert!(matches!(next, Err(CheckError::Poisoned(_))), "{next:?}");

    // Through a pop: the level is removed, and the error is of reach Session.
    let (b, _) = analyzer(true);
    let b = b.with_reassert_fault(true);
    let (_g, mut t) = loaded_w("stack_reassert_cannot_run_pop", &b);
    let mut fork = Stacked::fork(&b, &mut t).expect("fork");
    b.write(&mut t, &mut fork, &[w_edit("c/src/lib.rs", &err_fn("l1"))])
        .expect("L1");
    let popped = b.pop(&mut t, &mut fork);
    assert!(
        matches!(&popped, Err(e @ CheckError::Poisoned(_)) if e.reach() == CheckReach::Session),
        "{popped:?}"
    );
    assert_eq!(b.levels(&t, &fork), Ok(0), "the level is removed");
    let released = b.release(&mut t, fork);
    assert!(
        matches!(released, Err(CheckError::Poisoned(_))),
        "{released:?}"
    );
}

#[test]
fn a_database_write_that_panics_poisons_the_session() {
    // A write: refused as `Poisoned`, no level, the reads at the base.
    let (healthy, _) = analyzer(false);
    let (w, _) = analyzer(false);
    let w = w.with_db_write_panic(Some(DbWritePanic::Write));
    let (_f, mut s) = loaded_w("stack_db_write_panic", &healthy);
    let base = healthy.text(&s, &rel("a/src/lib.rs"));
    let provenance = healthy.provenance(&s);
    let mut fork = Stacked::fork(&healthy, &mut s).expect("fork");
    healthy
        .write(&mut s, &mut fork, &[w_edit("c/src/lib.rs", &err_fn("l1"))])
        .expect("L1");
    let written = w.write(&mut s, &mut fork, &[w_edit("a/src/lib.rs", &err_fn("l2"))]);
    assert!(
        matches!(&written, Err(e @ CheckError::Poisoned(_)) if e.reach() == CheckReach::Session),
        "{written:?}"
    );
    assert_eq!(healthy.levels(&s, &fork), Ok(1), "no level pushed");
    assert_eq!(healthy.text(&s, &rel("a/src/lib.rs")), base);
    assert_eq!(healthy.provenance(&s), provenance);
    let released = healthy.release(&mut s, fork);
    assert!(
        matches!(released, Err(CheckError::Poisoned(_))),
        "{released:?}"
    );

    // Through a check: the session error, not a Blueprint error.
    let (c, _) = analyzer(false);
    let c = c.with_db_write_panic(Some(DbWritePanic::Write));
    let (_g, mut t) = loaded("stack_db_write_panic_check", &c);
    let checked = c.check(&mut t, &[lib_edit("\npub fn h() {}\n")], Scope::Affected);
    assert!(
        matches!(&checked, Err(e @ CheckError::Poisoned(_)) if e.reach() == CheckReach::Session),
        "{checked:?}"
    );
    assert_eq!(c.text(&t, &lib()).as_deref(), Some(UNRESOLVED_LIB));

    // A revert: `Poisoned`, the level removed, the reads at the base.
    let (healthy, _) = analyzer(false);
    let (r, _) = analyzer(false);
    let r = r.with_db_write_panic(Some(DbWritePanic::Revert));
    let (_h, mut u) = loaded_w("stack_db_revert_panic", &healthy);
    let base = healthy.text(&u, &rel("a/src/lib.rs"));
    let mut fork = Stacked::fork(&healthy, &mut u).expect("fork");
    healthy
        .write(&mut u, &mut fork, &[w_edit("a/src/lib.rs", &err_fn("l1"))])
        .expect("L1");
    let popped = r.pop(&mut u, &mut fork);
    assert!(
        matches!(&popped, Err(e @ CheckError::Poisoned(_)) if e.reach() == CheckReach::Session),
        "{popped:?}"
    );
    assert_eq!(healthy.levels(&u, &fork), Ok(0), "the level is removed");
    assert_eq!(healthy.text(&u, &rel("a/src/lib.rs")), base);
    let next = Stacked::fork(&healthy, &mut u);
    assert!(matches!(next, Err(CheckError::Poisoned(_))), "{next:?}");
}

#[test]
fn pop_reassert_compares_with_the_level_below() {
    let (healthy, obs) = analyzer(true);
    let (faulty, _) = analyzer(true);
    let faulty =
        faulty.with_detector_fault(Some((GapDetector::UnresolvedPath, rel("a/src/lib.rs"))));
    let (_f, mut s) = loaded_w("stack_reassert", &healthy);
    let l1 = w_edit("a/src/lib.rs", &err_fn("l1"));
    let l2 = w_edit("a/src/x.rs", &err_fn("l2"));

    let mut fork = Stacked::fork(&healthy, &mut s).expect("fork");
    healthy
        .write(&mut s, &mut fork, std::slice::from_ref(&l1))
        .expect("L1");
    healthy
        .write(&mut s, &mut fork, std::slice::from_ref(&l2))
        .expect("L2");
    obs.take();
    assert!(healthy.pop(&mut s, &mut fork).expect("a healthy pop"));
    // L1 changed `a/src/lib.rs`, so the level below L2 is not the baseline: a comparison with the
    // baseline would not be `Some(true)`.
    assert_eq!(revert_oks(&obs.take()), vec![Some(true)]);

    healthy
        .write(&mut s, &mut fork, std::slice::from_ref(&l2))
        .expect("L2 again");
    let popped = faulty.pop(&mut s, &mut fork);
    assert!(
        matches!(popped, Err(CheckError::RevertMismatch { .. })),
        "{popped:?}"
    );
    let next = Stacked::fork(&healthy, &mut s);
    assert!(matches!(next, Err(CheckError::Poisoned(_))), "{next:?}");
}

#[test]
fn writes_count_for_the_sampled_reassert() {
    let (a, obs) = analyzer_every(2);
    let (_f, mut s) = loaded_w("stack_every", &a);
    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    a.write(&mut s, &mut fork, &[w_edit("c/src/lib.rs", &err_fn("l1"))])
        .expect("write 1");
    a.write(&mut s, &mut fork, &[w_edit("a/src/lib.rs", &err_fn("l2"))])
        .expect("write 2");
    obs.take();
    assert!(a.pop(&mut s, &mut fork).expect("pop L2"));
    assert_eq!(
        revert_oks(&obs.take()),
        vec![Some(true)],
        "write 2 is sampled"
    );
    assert!(a.pop(&mut s, &mut fork).expect("pop L1"));
    assert_eq!(revert_oks(&obs.take()), vec![None], "write 1 is not");
    a.release(&mut s, fork).expect("release");

    let edit = w_edit("a/src/lib.rs", &err_fn("c"));
    a.check(&mut s, std::slice::from_ref(&edit), Scope::Affected)
        .expect("write 3");
    assert_eq!(revert_oks(&obs.take()), vec![None], "write 3 is not");
    a.check(&mut s, std::slice::from_ref(&edit), Scope::Affected)
        .expect("write 4");
    assert_eq!(revert_oks(&obs.take()), vec![Some(true)], "write 4 is");
}

#[test]
fn stacked_writes_are_audited() {
    let (a, _) = analyzer(false);
    let (faulty, _) = analyzer(false);
    let faulty =
        faulty.with_detector_fault(Some((GapDetector::UnresolvedPath, rel("a/src/lib.rs"))));
    let (_f, mut s) = loaded_w("stack_audit", &a);
    // The control: nothing was written, so there is nothing to audit.
    assert_eq!(faulty.audit_revert(&mut s), Ok(()));

    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    a.write(&mut s, &mut fork, &[w_edit("a/src/lib.rs", &err_fn("l1"))])
        .expect("L1");
    a.release(&mut s, fork).expect("release");
    let audited = faulty.audit_revert(&mut s);
    assert!(
        matches!(audited, Err(CheckError::RevertMismatch { .. })),
        "{audited:?}"
    );
}

#[test]
fn pop_without_a_level_is_false() {
    let (a, _) = analyzer(false);
    let (_f, mut s) = loaded_w("stack_pop_none", &a);
    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    assert_eq!(a.pop(&mut s, &mut fork), Ok(false));
    assert_eq!(a.levels(&s, &fork), Ok(0));
    assert_eq!(
        a.diagnose(&s, &fork, Scope::Workspace),
        Ok(ScopedFindings::default())
    );
    assert_eq!(
        a.diagnose(&s, &fork, Scope::Affected),
        Ok(ScopedFindings::default())
    );
    for (path, text) in W_FILES {
        assert_eq!(
            a.fork_text(&s, &fork, &rel(path)),
            Ok(Some(text.to_owned()))
        );
        assert_eq!(a.text(&s, &rel(path)).as_deref(), Some(text));
    }
    a.release(&mut s, fork).expect("release");
}

#[test]
fn release_reverts_every_level() {
    let (a, obs) = analyzer(false);
    let (_f, mut s) = loaded_w("stack_release", &a);
    let provenance = a.provenance(&s);
    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    a.write(
        &mut s,
        &mut fork,
        &[
            w_edit("c/src/lib.rs", &err_fn("l1")),
            Edit::write(rel("c/src/n.rs"), "pub fn n() {}\n"),
        ],
    )
    .expect("L1");
    a.write(
        &mut s,
        &mut fork,
        &[
            Edit::delete(rel("a/src/x.rs")),
            w_edit("a/src/lib.rs", &err_fn("l2")),
        ],
    )
    .expect("L2");
    a.write(&mut s, &mut fork, &[w_edit("a/src/lib.rs", &err_fn("l3"))])
        .expect("L3");
    obs.take();

    a.release(&mut s, fork).expect("release");
    assert_eq!(revert_oks(&obs.take()).len(), 3, "one revert per level");
    for (path, text) in W_FILES {
        assert_eq!(a.text(&s, &rel(path)).as_deref(), Some(text), "{path}");
    }
    assert_eq!(a.text(&s, &rel("c/src/n.rs")), None);
    assert_eq!(a.provenance(&s), provenance);
    let again = Stacked::fork(&a, &mut s).expect("a new fork works");
    assert_eq!(a.levels(&s, &again), Ok(0));
    a.release(&mut s, again).expect("release");
}

#[test]
fn release_stops_writing_after_a_failure() {
    let (a, obs) = analyzer(false);
    let a = a.with_revert_fault(Some(rel("a/src/lib.rs")));
    let (_f, mut s) = loaded_w("stack_release_stops", &a);
    let provenance = a.provenance(&s);
    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    a.write(&mut s, &mut fork, &[w_edit("c/src/lib.rs", &err_fn("l1"))])
        .expect("L1");
    a.write(&mut s, &mut fork, &[w_edit("b/src/lib.rs", &err_fn("l2"))])
        .expect("L2");
    a.write(&mut s, &mut fork, &[w_edit("a/src/lib.rs", &err_fn("l3"))])
        .expect("L3");
    obs.take();

    let released = a.release(&mut s, fork);
    assert_eq!(
        released,
        Err(CheckError::RevertMismatch {
            paths: vec![rel("a/src/lib.rs")]
        })
    );
    // The top level's revert failed; the two below were not written.
    assert_eq!(revert_oks(&obs.take()).len(), 1);
    // The reads still describe the base, though the database holds the levels' texts.
    assert_eq!(a.text(&s, &rel("c/src/lib.rs")).as_deref(), Some(C_LIB));
    assert_eq!(a.text(&s, &rel("b/src/lib.rs")).as_deref(), Some(B_LIB));
    assert_eq!(a.text(&s, &rel("a/src/lib.rs")).as_deref(), Some(A_LIB));
    assert_eq!(a.provenance(&s), provenance);
    let next = Stacked::fork(&a, &mut s);
    assert!(matches!(next, Err(CheckError::Poisoned(_))), "{next:?}");
}

#[test]
fn poisoned_session_refuses_stacked_operations() {
    let (a, obs) = analyzer(false);
    let (_f, mut s) = loaded_w("stack_poisoned_ops", &a);
    let provenance = a.provenance(&s);
    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    a.write(
        &mut s,
        &mut fork,
        &[
            w_edit("c/src/lib.rs", &err_fn("l1")),
            Edit::write(rel("c/src/n.rs"), "pub fn n() {}\n"),
        ],
    )
    .expect("L1");
    a.write(
        &mut s,
        &mut fork,
        &[
            Edit::delete(rel("a/src/x.rs")),
            w_edit("a/src/lib.rs", &err_fn("l2")),
        ],
    )
    .expect("L2");
    s.poison("test");
    obs.take();

    let poisoned = |r: Result<(), CheckError>| matches!(r, Err(CheckError::Poisoned(_)));
    assert!(poisoned(a.write(&mut s, &mut fork, &[])));
    assert!(poisoned(a.diagnose(&s, &fork, Scope::Affected).map(|_| ())));
    for scope in [Scope::Affected, Scope::Workspace] {
        assert!(poisoned(a.files_of(&s, &fork, scope).map(|_| ())));
    }
    assert!(poisoned(
        a.fork_text(&s, &fork, &rel("a/src/lib.rs")).map(|_| ())
    ));
    assert!(poisoned(a.pop(&mut s, &mut fork).map(|_| ())));
    assert_eq!(a.levels(&s, &fork), Ok(2));

    assert!(poisoned(a.release(&mut s, fork)));
    assert!(
        obs.take().is_empty(),
        "a release on a poisoned session reverts nothing"
    );
    // The fork has ended and the reads describe the base.
    for (path, text) in W_FILES {
        assert_eq!(a.text(&s, &rel(path)).as_deref(), Some(text), "{path}");
    }
    assert_eq!(a.text(&s, &rel("c/src/n.rs")), None);
    assert_eq!(a.provenance(&s), provenance);
    assert!(poisoned(Stacked::fork(&a, &mut s).map(|_| ())));
}

#[test]
fn session_operations_refuse_while_a_fork_is_live() {
    let target = [Target::new(rel("a/src/lib.rs"), None)];
    let edit = w_edit("a/src/lib.rs", &err_fn("c"));

    // A live stacked fork with one level (write 1 of a sampling every 2nd).
    let (a, obs) = analyzer_every(2);
    let (_f, mut s) = loaded_w("stack_live_stacked", &a);
    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    a.write(&mut s, &mut fork, &[w_edit("c/src/lib.rs", &err_fn("l1"))])
        .expect("L1");
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
    assert!(matches!(
        Stacked::fork(&a, &mut s),
        Err(CheckError::ForkInUse)
    ));
    assert_eq!(a.levels(&s, &fork), Ok(1), "the fork was not touched");
    a.release(&mut s, fork).expect("release");
    // Nothing refused was counted: this check is write 2, and it is sampled.
    obs.take();
    a.check(&mut s, std::slice::from_ref(&edit), Scope::Affected)
        .expect("write 2");
    assert_eq!(revert_oks(&obs.take()), vec![Some(true)]);

    // The same with a live `Analyzer` fork, whose write is not counted.
    let (a, obs) = analyzer_every(2);
    let (_f2, mut s) = loaded_w("stack_live_analyzer", &a);
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
        Stacked::fork(&a, &mut s),
        Err(CheckError::ForkInUse)
    ));
    assert!(matches!(
        Analyzer::fork(&a, &mut s),
        Err(AnalyzerError::ForkInUse)
    ));
    Analyzer::discard(&a, &mut s, fork).expect("discard");
    obs.take();
    a.check(&mut s, std::slice::from_ref(&edit), Scope::Affected)
        .expect("write 1");
    a.check(&mut s, std::slice::from_ref(&edit), Scope::Affected)
        .expect("write 2");
    assert_eq!(revert_oks(&obs.take()), vec![None, Some(true)]);
}

#[test]
fn verifier_reads_describe_the_base_while_a_fork_is_live() {
    let (a, _) = analyzer(false);
    let (_f, mut s) = loaded_w("stack_reads", &a);
    let provenance = a.provenance(&s);
    let baseline = Verifier::baseline(&a, &s);
    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    a.write(
        &mut s,
        &mut fork,
        &[
            w_edit("c/src/lib.rs", &err_fn("l1")),
            Edit::write(rel("c/src/n.rs"), "pub fn n() {}\n"),
        ],
    )
    .expect("L1");
    a.write(
        &mut s,
        &mut fork,
        &[
            Edit::delete(rel("a/src/x.rs")),
            w_edit("a/src/lib.rs", &err_fn("l2")),
        ],
    )
    .expect("L2");

    for (path, text) in W_FILES {
        assert_eq!(a.text(&s, &rel(path)).as_deref(), Some(text), "{path}");
    }
    assert_eq!(a.text(&s, &rel("c/src/n.rs")), None);
    assert_eq!(a.provenance(&s), provenance);
    assert_eq!(Verifier::baseline(&a, &s), baseline);
    // The fork itself reads the level.
    assert_eq!(a.fork_text(&s, &fork, &rel("a/src/x.rs")), Ok(None));
    assert!(matches!(
        a.fork_text(&s, &fork, &rel("c/src/n.rs")),
        Ok(Some(_))
    ));
    a.release(&mut s, fork).expect("release");
}

#[test]
fn a_ticket_of_another_session_is_refused() {
    let (a, _) = analyzer(false);
    let (_fa, mut sa) = loaded_w("stack_ticket_wa", &a);
    let (_fb, mut sb) = loaded_w("stack_ticket_wb", &a);
    let mut ta = Stacked::fork(&a, &mut sa).expect("fork a");
    let mut tb = Stacked::fork(&a, &mut sb).expect("fork b");
    let edit = w_edit("a/src/lib.rs", &err_fn("l1"));
    a.write(&mut sa, &mut ta, std::slice::from_ref(&edit))
        .expect("a L1");
    a.write(&mut sb, &mut tb, std::slice::from_ref(&edit))
        .expect("b L1");
    let path = rel("a/src/lib.rs");
    let text = format!("{A_LIB}{}", err_fn("l1"));

    // Every operation with the other session's ticket is refused, and changes nothing.
    let refused_by = |s: &mut RaSession, other: &RaStack| {
        let mut other = other.clone();
        let is_refused = |r: Result<(), CheckError>| r == Err(CheckError::ForkInUse);
        assert!(is_refused(a.write(
            s,
            &mut other,
            std::slice::from_ref(&edit)
        )));
        assert!(is_refused(a.levels(s, &other).map(|_| ())));
        assert!(is_refused(
            a.diagnose(s, &other, Scope::Affected).map(|_| ())
        ));
        assert!(is_refused(a.fork_text(s, &other, &path).map(|_| ())));
        assert!(is_refused(a.pop(s, &mut other).map(|_| ())));
        assert!(is_refused(a.release(s, other.clone())));
    };
    refused_by(&mut sa, &tb);
    refused_by(&mut sb, &ta);
    // Also on a poisoned session: the ticket is looked at before the poison.
    sa.poison("test");
    refused_by(&mut sa, &tb);
    // Its own ticket on the poisoned session is `Poisoned`.
    assert!(matches!(
        a.write(&mut sa, &mut ta, &[]),
        Err(CheckError::Poisoned(_))
    ));

    // Nothing changed on either: each fork still has its level and its text.
    assert_eq!(a.levels(&sa, &ta), Ok(1));
    assert_eq!(a.levels(&sb, &tb), Ok(1));
    assert_eq!(a.fork_text(&sb, &tb, &path), Ok(Some(text)));
    // The healthy session's own release still works.
    a.release(&mut sb, tb).expect("release b");
    assert_eq!(a.text(&sb, &path).as_deref(), Some(A_LIB));
    assert!(matches!(
        a.release(&mut sa, ta),
        Err(CheckError::Poisoned(_))
    ));
}

#[test]
fn one_level_matches_the_analyzer_path() {
    let (a, _) = analyzer(false);
    let (_f, mut s) = loaded_w("stack_analyzer_path", &a);
    let edits = vec![
        w_edit("a/src/lib.rs", &err_fn("l1")),
        Edit::write(rel("a/src/n.rs"), "pub fn n() {\n    1u32.nope_n();\n}\n"),
        Edit::delete(rel("a/src/x.rs")),
    ];

    for scope in [Scope::Affected, Scope::Workspace] {
        let mut lower = Analyzer::fork(&a, &mut s).expect("fork");
        Analyzer::apply(&a, &mut s, &mut lower, &edits).expect("apply");
        let via_analyzer = Analyzer::diagnostics(&a, &s, &lower, scope).expect("diagnostics");
        Analyzer::discard(&a, &mut s, lower).expect("discard");
        let after_analyzer: Vec<Option<String>> = ["a/src/lib.rs", "a/src/x.rs", "a/src/n.rs"]
            .iter()
            .map(|p| a.text(&s, &rel(p)))
            .collect();

        let mut upper = Stacked::fork(&a, &mut s).expect("fork");
        a.write(&mut s, &mut upper, &edits).expect("write");
        let via_stacked = a.diagnose(&s, &upper, scope).expect("diagnose");
        a.release(&mut s, upper).expect("release");
        let after_stacked: Vec<Option<String>> = ["a/src/lib.rs", "a/src/x.rs", "a/src/n.rs"]
            .iter()
            .map(|p| a.text(&s, &rel(p)))
            .collect();

        assert_eq!(via_analyzer.files, via_stacked.files, "{scope:?}");
        assert_eq!(via_analyzer.findings, via_stacked.findings, "{scope:?}");
        assert_eq!(after_analyzer, after_stacked, "{scope:?}");
        assert_eq!(
            after_stacked,
            vec![Some(A_LIB.to_owned()), Some(A_X.to_owned()), None]
        );
    }
}

/// What `Verifier::check` is said to be: a fork, one write, a diagnosis and a release, the
/// release's failure winning, then the write's, then the diagnosis's.
fn stacked_check(
    a: &RaAnalyzer,
    s: &mut RaSession,
    edits: &[Edit],
    scope: Scope,
) -> Result<ScopedFindings, CheckError> {
    let mut fork = Stacked::fork(a, s)?;
    let written = a.write(s, &mut fork, edits);
    let diagnosed = written.is_ok().then(|| a.diagnose(s, &fork, scope));
    let released = a.release(s, fork);
    released?;
    written?;
    diagnosed.unwrap_or(Ok(ScopedFindings::default()))
}

/// The checks and reverts among `events`, without their times.
fn event_shapes(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Checked {
                scope_files,
                total_files,
                ..
            } => Some(format!("checked {scope_files}/{total_files}")),
            Event::Reverted { revert_ok, .. } => Some(format!("reverted {revert_ok:?}")),
            _ => None,
        })
        .collect()
}

fn w_analyzer(
    timeout: Option<u64>,
    fault: Option<(GapDetector, &str)>,
) -> (RaAnalyzer, Arc<RecordingObserver>) {
    let (a, obs) = analyzer(false);
    let a = a
        .with_check_timeout(timeout)
        .with_detector_fault(fault.map(|(d, p)| (d, rel(p))));
    (a, obs)
}

#[test]
fn check_is_the_one_level_case() {
    let f = w_fixture("stack_one_level");
    let (healthy, _) = w_analyzer(None, None);
    let mut checked = Analyzer::load(&healthy, &f.dir).expect("load");
    let mut stacked = Analyzer::load(&healthy, &f.dir).expect("load");
    let m = "a/src/m.rs";
    let plain = vec![w_edit("a/src/lib.rs", &err_fn("p"))];
    let new_file = vec![Edit::write(rel(m), "pub fn m() {}\n")];

    /// A name, edits, a cut-off and a detector fault.
    type Scenario<'a> = (
        &'a str,
        Vec<Edit>,
        Option<u64>,
        Option<(GapDetector, &'a str)>,
    );
    let scenarios: Vec<Scenario> = vec![
        ("a plain edit", plain.clone(), None, None),
        ("a cut-off", plain, Some(0), None),
        (
            "a new detector panic",
            new_file,
            None,
            Some((GapDetector::UnresolvedPath, m)),
        ),
    ];
    for (name, edits, timeout, fault) in scenarios {
        let (a1, o1) = w_analyzer(timeout, fault);
        let (a2, o2) = w_analyzer(timeout, fault);
        o1.take();
        o2.take();
        let one = a1.check(&mut checked, &edits, Scope::Affected);
        let two = stacked_check(&a2, &mut stacked, &edits, Scope::Affected);
        assert_eq!(one, two, "{name}: result");
        let (shapes1, shapes2) = (event_shapes(&o1.take()), event_shapes(&o2.take()));
        assert_eq!(shapes1, shapes2, "{name}: events");
        // A cut-off diagnosis records a revert only; the others a check, then a revert.
        let want: &[&str] = if timeout.is_some() {
            &["reverted None"]
        } else {
            &["checked", "reverted None"]
        };
        assert_eq!(shapes1.len(), want.len(), "{name}: {shapes1:?}");
        for (shape, want) in shapes1.iter().zip(want) {
            assert!(shape.starts_with(want), "{name}: {shapes1:?}");
        }
        for e in &edits {
            assert_eq!(
                a1.text(&checked, &e.path),
                a2.text(&stacked, &e.path),
                "{name}: text of {}",
                e.path
            );
        }
    }

    // A detector that panicked at the baseline too: loaded with the fault.
    let fault = Some((GapDetector::UnresolvedPath, "a/src/x.rs"));
    let (a1, o1) = w_analyzer(None, fault);
    let (a2, o2) = w_analyzer(None, fault);
    let mut checked = Analyzer::load(&a1, &f.dir).expect("load");
    let mut stacked = Analyzer::load(&a2, &f.dir).expect("load");
    let edits = vec![w_edit("a/src/lib.rs", &err_fn("p"))];
    o1.take();
    o2.take();
    let one = a1.check(&mut checked, &edits, Scope::Affected);
    let two = stacked_check(&a2, &mut stacked, &edits, Scope::Affected);
    assert_eq!(one, two, "a baseline detector panic: result");
    let named = one.expect("a baseline panic is named, not failed");
    assert!(named.unseen().contains(&UnseenClass::UnresolvedPath));
    let (shapes1, shapes2) = (event_shapes(&o1.take()), event_shapes(&o2.take()));
    assert_eq!(shapes1, shapes2, "a baseline detector panic: events");
    assert!(
        matches!(shapes1.as_slice(), [c, r] if c.starts_with("checked") && r.starts_with("reverted")),
        "{shapes1:?}"
    );
}

#[test]
fn a_poisoned_session_is_poisoned_on_the_analyzer_port() {
    let (a, _) = analyzer(false);
    let (_f, mut s) = loaded("stack_poisoned_port", &a);
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
                assert_eq!(e.reach(), ryo_v2_core::app::ports::CheckReach::Session);
            }
            other => panic!("expected the session's poison, got {other:?}"),
        }
    }
    assert_eq!(a.text(&s, &lib()).as_deref(), Some(UNRESOLVED_LIB));
}

/// A clock that moves by `step` on every reading and by what a test adds with `advance`.
struct StepClock {
    now: std::sync::atomic::AtomicU64,
    step: u64,
}

impl StepClock {
    fn advance(&self, ms: u64) {
        self.now.fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
    }
}

impl ryo_v2_core::app::ports::Clock for StepClock {
    fn now_ms(&self) -> u64 {
        self.now
            .fetch_add(self.step, std::sync::atomic::Ordering::SeqCst)
    }
}

#[test]
fn the_diagnoses_of_a_level_do_not_add_up() {
    const IDLE: u64 = 100_000;
    let clock = Arc::new(StepClock {
        now: std::sync::atomic::AtomicU64::new(0),
        step: 1,
    });
    let observer = Arc::new(RecordingObserver::default());
    let a = RaAnalyzer::new(
        RaConfig {
            proc_macro: false,
            threads: Some(1),
            assert_revert: false,
            gap_detectors: true,
            assert_revert_every: None,
            cfg_test: false,
        },
        clock.clone(),
        observer.clone(),
    );
    let (_f, mut s) = loaded("stack_check_ms", &a);
    observer.take();
    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    let mut check_ms = Vec::new();
    let diagnose = |a: &RaAnalyzer, s: &RaSession, fork: &RaStack| {
        clock.advance(IDLE);
        a.diagnose(s, fork, Scope::Affected).expect("diagnose");
        clock.advance(IDLE);
        // The events since the last diagnosis: a pop's revert is among them.
        let events = observer.take();
        let checked: Vec<u64> = events
            .iter()
            .filter_map(|e| match e {
                Event::Checked { check_ms, .. } => Some(*check_ms),
                _ => None,
            })
            .collect();
        assert_eq!(
            checked.len(),
            1,
            "one diagnosis records one check: {events:?}"
        );
        checked[0]
    };

    a.write(&mut s, &mut fork, &[lib_edit("\npub fn h() {}\n")])
        .expect("L1");
    check_ms.push(diagnose(&a, &s, &fork));
    // The same level again: the first diagnosis and the idle time are not in it.
    check_ms.push(diagnose(&a, &s, &fork));
    a.write(&mut s, &mut fork, &[lib_edit("\npub fn i() {}\n")])
        .expect("L2");
    check_ms.push(diagnose(&a, &s, &fork));
    // The level below after a pop: the time of the pop and of the idle is not in it.
    a.pop(&mut s, &mut fork).expect("pop");
    check_ms.push(diagnose(&a, &s, &fork));
    a.release(&mut s, fork).expect("release");

    // Every diagnosis is the write of its level plus itself, which cost the same here.
    assert!(check_ms.iter().all(|ms| *ms == check_ms[0]), "{check_ms:?}");
    assert!(check_ms[0] > 0 && check_ms[0] < IDLE, "{check_ms:?}");
}

/// Every class of `found` names at least one file, and only files of `found`.
fn assert_classes_name_their_files(found: &ScopedFindings, what: &str) {
    for (class, on) in &found.unseen_at {
        assert!(!on.is_empty(), "{what}: {class:?} names no file");
        assert!(
            on.iter().all(|p| found.files.contains(p)),
            "{what}: {class:?} names {on:?}, outside {:?}",
            found.files
        );
    }
}

#[test]
fn a_diagnosis_names_where_it_observed_each_unseen_class() {
    let x = rel("a/src/x.rs");

    // A macro call that does not expand: the detector is skipped on that file alone.
    let (a, _) = analyzer(false);
    let f = w_fixture("stack_unseen_at_macro");
    let mut s = Analyzer::load(&a, &f.dir).expect("load");
    let mut twin = Analyzer::load(&a, &f.dir).expect("load the twin");
    let edits = vec![w_edit("a/src/x.rs", "not_a_macro!();\n")];
    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    a.write(&mut s, &mut fork, &edits).expect("L1");
    for scope in [Scope::Affected, Scope::Workspace] {
        let found = a.diagnose(&s, &fork, scope).expect("diagnose");
        assert_eq!(
            found.unseen_at,
            BTreeMap::from([(UnseenClass::UnexpandedMacro, BTreeSet::from([x.clone()]))]),
            "macro, {scope:?}"
        );
        assert_classes_name_their_files(&found, "macro, diagnose");
        let checked = a.check(&mut twin, &edits, scope).expect("check");
        assert_eq!(
            checked.unseen_at, found.unseen_at,
            "macro, check, {scope:?}"
        );
        assert_classes_name_their_files(&checked, "macro, check");
    }
    a.release(&mut s, fork).expect("release");

    // A detector that panicked on a file at the baseline: named with that file, at a level that
    // edits another file of the crate.
    let (b, _) = analyzer(false);
    let b = b.with_detector_fault(Some((GapDetector::UnresolvedPath, x.clone())));
    let g = w_fixture("stack_unseen_at_panic");
    let mut s = Analyzer::load(&b, &g.dir).expect("load");
    let mut twin = Analyzer::load(&b, &g.dir).expect("load the twin");
    let edits = vec![w_edit("a/src/lib.rs", &err_fn("l1"))];
    let mut fork = Stacked::fork(&b, &mut s).expect("fork");
    b.write(&mut s, &mut fork, &edits).expect("L1");
    for scope in [Scope::Affected, Scope::Workspace] {
        let found = b.diagnose(&s, &fork, scope).expect("diagnose");
        assert_eq!(
            found.unseen_at,
            BTreeMap::from([(UnseenClass::UnresolvedPath, BTreeSet::from([x.clone()]))]),
            "panic, {scope:?}"
        );
        assert_classes_name_their_files(&found, "panic, diagnose");
        let checked = b.check(&mut twin, &edits, scope).expect("check");
        assert_eq!(
            checked.unseen_at, found.unseen_at,
            "panic, check, {scope:?}"
        );
        assert_classes_name_their_files(&checked, "panic, check");
    }
    b.release(&mut s, fork).expect("release");
}

/// The edits that take the base of W to the state after `levels`, as a check is given them: per
/// path the last edit wins; a write equal to the base text and a delete of a path the base lacks
/// give no edit.
fn net(levels: &[Vec<Edit>]) -> Vec<Edit> {
    let mut last: BTreeMap<RelPath, Option<String>> = BTreeMap::new();
    for edit in levels.iter().flatten() {
        last.insert(edit.path.clone(), edit.new_text.clone());
    }
    last.into_iter()
        .filter_map(|(path, text)| {
            let base = W_FILES
                .iter()
                .find(|(p, _)| *p == path.as_str())
                .map(|(_, t)| *t);
            match (text, base) {
                (Some(text), Some(base)) if text == base => None,
                (Some(text), _) => Some(Edit::write(path, text)),
                (None, Some(_)) => Some(Edit::delete(path)),
                (None, None) => None,
            }
        })
        .collect()
}

/// The paths `edits` delete.
fn deleted_by(edits: &[Edit]) -> Vec<RelPath> {
    edits
        .iter()
        .filter(|e| e.new_text.is_none())
        .map(|e| e.path.clone())
        .collect()
}

#[test]
fn files_of_and_of_level_equal_the_check_over_a_stack() {
    let (a, _) = analyzer(false);
    let f = w_fixture("stack_files_of");
    let mut s = Analyzer::load(&a, &f.dir).expect("load the stacked session");
    let mut twin = Analyzer::load(&a, &f.dir).expect("load the flat twin");
    let baseline = Verifier::baseline(&a, &s);
    let z = "c/src/z.rs";
    let mut all = sequences();
    // Changed, then written back to base.
    all.push(vec![
        vec![w_edit(z, &err_fn("z1"))],
        vec![Edit::write(rel(z), C_Z)],
    ]);
    // Written back to base at a lower level, then changed again.
    all.push(vec![
        vec![w_edit(z, &err_fn("z1"))],
        vec![Edit::write(rel(z), C_Z)],
        vec![w_edit(z, &err_fn("z3"))],
    ]);

    for scope in [Scope::Affected, Scope::Workspace] {
        // A fork with no level: the files of a check with no edit (none at Affected).
        let fork = Stacked::fork(&a, &mut s).expect("fork");
        let none = a
            .files_of(&s, &fork, scope)
            .expect("files_of with no level");
        let flat = a
            .check(&mut twin, &[], scope)
            .expect("a check with no edit");
        assert_eq!(none, flat.files, "no level, {scope:?}");
        if matches!(scope, Scope::Affected) {
            assert!(none.is_empty(), "{none:?}");
        }
        a.release(&mut s, fork).expect("release");

        for (n, levels) in all.iter().enumerate() {
            let mut fork = Stacked::fork(&a, &mut s).expect("fork");
            let mut below = ScopedFindings::default();
            for (k, edits) in levels.iter().enumerate() {
                let at = format!("sequence {n}, level {}, {scope:?}", k + 1);
                a.write(&mut s, &mut fork, edits)
                    .unwrap_or_else(|e| panic!("{at}: {e:?}"));
                let level = a.diagnose(&s, &fork, scope).expect("diagnose");
                let files = a.files_of(&s, &fork, scope).expect("files_of");
                let flat_edits = net(&levels[..=k]);
                let flat = a
                    .check(&mut twin, &flat_edits, scope)
                    .unwrap_or_else(|e| panic!("{at}: the flat check: {e:?}"));
                assert_eq!(files, flat.files, "{at}: files_of");
                if flat_edits.is_empty() && matches!(scope, Scope::Affected) {
                    assert!(files.is_empty(), "{at}: every path is back at base");
                }

                let (verdict, found) =
                    Verdict::of_level(&baseline, &below, &level, &deleted_by(edits), &files);
                assert_eq!(found, flat, "{at}: findings");
                let (want, _) = Verdict::of_level(
                    &baseline,
                    &ScopedFindings::default(),
                    &flat,
                    &[],
                    &flat.files,
                );
                assert_eq!(verdict, want, "{at}: verdict");
                below = found;
            }
            a.release(&mut s, fork).expect("release");
        }
    }
}

/// Every finding of `found` is on one of its files.
fn assert_findings_on_their_files(found: &ScopedFindings, what: &str) {
    for finding in &found.findings {
        assert!(
            found.files.contains(&finding.path),
            "{what}: {finding:?} is on no file of {:?}",
            found.files
        );
    }
}

#[test]
fn a_finding_is_on_a_file_of_its_diagnosis() {
    // Crate a pulls part.rs in with `include!`, and part.rs reads a constant lib.rs defines:
    // removing it from lib.rs gives an error rust-analyzer reports in part.rs, a file no level
    // edits. Crate c is apart from a.
    const LIB: &str = "include!(\"part.rs\");\npub const MISSING_VALUE: u32 = 0;\n";
    const LIB_DROPPED: &str = "include!(\"part.rs\");\n";
    const LIB_FIXED: &str = "include!(\"part.rs\");\npub const MISSING_VALUE: u32 = 1;\n";
    const PART: &str = "// part\npub fn g() -> u32 {\n    MISSING_VALUE\n}\n";
    const C: &str = "pub fn cf() {}\n";
    let manifest = |name: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
    };
    let (a_cargo, c_cargo) = (manifest("ia"), manifest("ic"));
    let fixture = |test: &str| {
        Fixture::new(
            test,
            &[
                (
                    "Cargo.toml",
                    "[workspace]\nmembers = [\"a\", \"c\"]\nresolver = \"2\"\n",
                ),
                ("a/Cargo.toml", &a_cargo),
                ("c/Cargo.toml", &c_cargo),
                ("a/src/lib.rs", LIB),
                ("a/src/part.rs", PART),
                ("c/src/lib.rs", C),
            ],
        )
    };
    let (lib, part, c) = (
        rel("a/src/lib.rs"),
        rel("a/src/part.rs"),
        rel("c/src/lib.rs"),
    );
    let c_err = format!("{C}{}", err_fn("c2"));
    // L1 drops the constant, L2 edits only crate c, L3 brings a constant back.
    let levels = [
        vec![Edit::write(lib.clone(), LIB_DROPPED)],
        vec![Edit::write(c.clone(), c_err.clone())],
        vec![Edit::write(lib.clone(), LIB_FIXED)],
    ];
    let flat_edits = [
        vec![Edit::write(lib.clone(), LIB_DROPPED)],
        vec![
            Edit::write(lib.clone(), LIB_DROPPED),
            Edit::write(c.clone(), c_err.clone()),
        ],
        vec![
            Edit::write(lib.clone(), LIB_FIXED),
            Edit::write(c.clone(), c_err.clone()),
        ],
    ];
    let on_part = |found: &ScopedFindings| found.findings.iter().any(|f| f.path == part);

    let (a, _) = analyzer(false);
    let f = fixture("stack_finding_on_its_files");
    let mut s = Analyzer::load(&a, &f.dir).expect("load the stacked session");
    let mut twin = Analyzer::load(&a, &f.dir).expect("load the flat twin");
    let baseline = Verifier::baseline(&a, &s);
    assert!(
        baseline.iter().all(|b| b.path != part),
        "no error in part.rs at the base: {baseline:?}"
    );

    for scope in [Scope::Affected, Scope::Workspace] {
        let mut fork = Stacked::fork(&a, &mut s).expect("fork");
        let mut below = ScopedFindings::default();
        for (k, edits) in levels.iter().enumerate() {
            let at = format!("level {}, {scope:?}", k + 1);
            a.write(&mut s, &mut fork, edits)
                .unwrap_or_else(|e| panic!("{at}: {e:?}"));
            let level = a.diagnose(&s, &fork, scope).expect("diagnose");
            assert_findings_on_their_files(&level, &format!("{at}, diagnose"));
            let flat = a
                .check(&mut twin, &flat_edits[k], scope)
                .unwrap_or_else(|e| panic!("{at}: the flat check: {e:?}"));
            assert_findings_on_their_files(&flat, &format!("{at}, check"));

            match k {
                // The error of the edit of lib.rs is on part.rs, at the level and in the check.
                0 => {
                    assert!(on_part(&level), "{at}: {:?}", level.findings);
                    assert!(on_part(&flat), "{at}: {:?}", flat.findings);
                }
                // Crate a is outside the top scope at Affected: its error is the level below's.
                1 => assert_eq!(
                    on_part(&level),
                    matches!(scope, Scope::Workspace),
                    "{at}: {:?}",
                    level.findings
                ),
                // The fix of lib.rs clears part.rs.
                _ => {
                    assert!(!on_part(&level), "{at}: {:?}", level.findings);
                    assert!(!on_part(&flat), "{at}: {:?}", flat.findings);
                }
            }

            let files = a.files_of(&s, &fork, scope).expect("files_of");
            let (verdict, found) = Verdict::of_level(&baseline, &below, &level, &[], &files);
            assert_eq!(found, flat, "{at}: findings");
            assert_eq!(
                verdict,
                Verdict::of_check(&baseline, &flat),
                "{at}: verdict"
            );
            assert_eq!(on_part(&found), k < 2, "{at}: the composed findings");
            below = found;
        }
        a.release(&mut s, fork).expect("release");
    }
}

#[test]
fn a_failed_base_read_is_a_session_error() {
    let z = rel("c/src/z.rs");
    let (a, _) = analyzer(false);
    let a = a.with_db_read_fault(Some(z.clone()));
    let (_f, mut s) = loaded_w("stack_failed_base_read", &a);

    // L1 changes z, L2 writes its base text back: whether z is an edit of the state needs its
    // base text.
    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    a.write(&mut s, &mut fork, &[w_edit("c/src/z.rs", &err_fn("z1"))])
        .expect("L1");
    a.write(&mut s, &mut fork, &[Edit::write(z.clone(), C_Z)])
        .expect("L2");
    let failed = a.files_of(&s, &fork, Scope::Affected);
    assert!(
        matches!(&failed, Err(e @ CheckError::Load(_)) if e.reach() == CheckReach::Session),
        "{failed:?}"
    );

    // Not poisoned: the fork, its levels and its texts are as before, and it is used and ends.
    assert_eq!(a.levels(&s, &fork), Ok(2));
    assert_eq!(a.fork_text(&s, &fork, &z), Ok(Some(C_Z.to_owned())));
    a.diagnose(&s, &fork, Scope::Affected)
        .expect("a diagnosis still runs");
    assert!(a.pop(&mut s, &mut fork).expect("pop"));
    assert_eq!(a.levels(&s, &fork), Ok(1));
    let stale = fork.clone();
    a.release(&mut s, fork).expect("release");
    assert_eq!(
        a.files_of(&s, &stale, Scope::Affected),
        Err(CheckError::ForkInUse),
        "a ticket that is not the live fork's"
    );

    // The boundary of the deferral (AR-014): the reads of `Verifier` map the failure as today.
    assert_eq!(a.text(&s, &z), None);
    assert!(!a.provenance(&s).files().contains_key(&z));
    a.check(
        &mut s,
        &[w_edit("a/src/lib.rs", &err_fn("c"))],
        Scope::Affected,
    )
    .expect("the session is not poisoned");
}
