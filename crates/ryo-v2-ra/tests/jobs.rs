//! Chains of `Forks` jobs against the flat check, on throwaway workspaces (the corpus is never
//! touched). A level of a chain is a job on the previous Layer (the root is `Forks::open`), and
//! its outcome is what `Forks::next` gives.

mod common;

use common::{Fixture, rel};
use ryo_v2_core::app::ports::{
    Analyzer, AnalyzerError, CheckError, CheckReach, Checked, Forks, Limit, Next, RevertAudit,
    Verifier,
};
use ryo_v2_core::domain::baseline::restrict;
use ryo_v2_core::domain::{Edit, RelPath, Scope, ScopedFindings, UnseenClass, Verdict};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_ra::{DbWritePanic, ForksConfig, GapDetector, RaAnalyzer, RaConfig, RaLayer, RaSession};
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// How long a test waits for one outcome before it calls the jobs blocked.
const WAIT: Duration = Duration::from_secs(120);

/// An analyzer on the Engine with forks of width 1 over two threads, the caller only waiting.
fn analyzer() -> RaAnalyzer {
    RaAnalyzer::new(
        RaConfig {
            proc_macro: false,
            threads: Some(2),
            gap_detectors: true,
            cfg_test: false,
            forks: ForksConfig {
                width: NonZeroUsize::new(1),
                caller_works: false,
            },
            ..RaConfig::default()
        },
        Arc::new(MonotonicClock::new()),
        Arc::new(RecordingObserver::default()),
    )
}

/// The outcome of one job of `edits` on `parent`, taken with `next`.
fn job_with(
    a: &RaAnalyzer,
    s: &mut RaSession,
    parent: &RaLayer,
    edits: &[Edit],
    scope: Scope,
    limit: Limit,
) -> Result<Checked<RaLayer>, CheckError> {
    let id = a.submit(s, parent, edits, scope, limit).expect("submit");
    match a.next(s, WAIT).expect("next") {
        Next::Done(got, result) if got == id => result,
        other => panic!("{other:?}"),
    }
}

/// [`job_with`] within the adapter's check cut-off.
fn job(
    a: &RaAnalyzer,
    s: &mut RaSession,
    parent: &RaLayer,
    edits: &[Edit],
    scope: Scope,
) -> Result<Checked<RaLayer>, CheckError> {
    job_with(a, s, parent, edits, scope, Limit::Check)
}

/// A job that is expected to run to its end.
fn ok(
    a: &RaAnalyzer,
    s: &mut RaSession,
    parent: &RaLayer,
    edits: &[Edit],
    scope: Scope,
) -> Checked<RaLayer> {
    job(a, s, parent, edits, scope).unwrap_or_else(|e| panic!("{e:?}"))
}

/// The diagnosis of every file of `layer`'s state: the findings of a job with no edit at
/// `Scope::Workspace` on it.
fn workspace_of(a: &RaAnalyzer, s: &mut RaSession, layer: &RaLayer) -> ScopedFindings {
    ok(a, s, layer, &[], Scope::Workspace).findings
}

fn text_at(a: &RaAnalyzer, s: &RaSession, layer: &RaLayer, path: &RelPath) -> Option<String> {
    a.layer_text(s, layer, path).expect("layer_text")
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

fn w_paths() -> Vec<RelPath> {
    W_FILES.iter().map(|(p, _)| rel(p)).collect()
}

fn w_paths_sorted() -> Vec<RelPath> {
    let mut paths = w_paths();
    paths.sort();
    paths
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
fn a_chain_of_jobs_diagnoses_as_its_flat_composition() {
    let a = analyzer();
    let f = w_fixture("jobs_flat");
    let mut s = Analyzer::load(&a, &f.dir).expect("load the forked session");
    let mut twin = Analyzer::load(&a, &f.dir).expect("load the flat twin");

    for (n, levels) in sequences().iter().enumerate() {
        let mut parent = a.open(&mut s).expect("open");
        for (k, edits) in levels.iter().enumerate() {
            let at = format!("sequence {n}, level {}", k + 1);
            let flat_edits = compose(&levels[..=k]);
            let flat = a
                .check(&mut twin, &flat_edits, Scope::Workspace)
                .unwrap_or_else(|e| panic!("{at}: the flat check: {e:?}"));
            let affected = ok(&a, &mut s, &parent, edits, Scope::Affected);
            let level = ok(&a, &mut s, &parent, edits, Scope::Workspace);
            assert_eq!(level.findings, flat, "{at}: Workspace");
            assert!(
                affected
                    .findings
                    .files
                    .iter()
                    .all(|p| flat.files.contains(p)),
                "{at}: the affected files are files of the state"
            );
            assert_eq!(
                affected.findings.findings,
                restrict(&flat.findings, &affected.findings.files),
                "{at}: Affected"
            );
            parent = level.layer;
        }
        drop(parent);
        a.close(&mut s).expect("close");
    }
}

#[test]
fn a_job_writes_against_its_parents_state() {
    let a = analyzer();
    let (_f, mut s) = loaded_w("jobs_resolution", &a);
    let m = rel("a/src/m.rs");
    let x = rel("a/src/x.rs");
    let lib_m = format!("{A_LIB}pub mod m;\n");

    let root = a.open(&mut s).expect("open");
    // L1 creates `a/src/m.rs` (with its `mod`).
    let l1 = ok(
        &a,
        &mut s,
        &root,
        &[
            Edit::write(rel("a/src/lib.rs"), lib_m),
            Edit::write(m.clone(), "pub fn m1() {}\n"),
        ],
        Scope::Affected,
    )
    .layer;
    assert_eq!(
        text_at(&a, &s, &l1, &m).as_deref(),
        Some("pub fn m1() {}\n")
    );
    // L2 modifies the file L1 created.
    let l2 = ok(
        &a,
        &mut s,
        &l1,
        &[Edit::write(m.clone(), "pub fn m2() {}\n")],
        Scope::Affected,
    )
    .layer;
    assert_eq!(
        text_at(&a, &s, &l2, &m).as_deref(),
        Some("pub fn m2() {}\n")
    );
    // L3 deletes it, and a load-time file with it.
    let l3 = ok(
        &a,
        &mut s,
        &l2,
        &[Edit::delete(m.clone()), Edit::delete(x.clone())],
        Scope::Affected,
    )
    .layer;
    assert_eq!(text_at(&a, &s, &l3, &m), None);
    assert_eq!(text_at(&a, &s, &l3, &x), None);
    // Deleting a path that is not a file of the state below is refused, a never-known path too.
    for gone in [m.clone(), x.clone(), rel("a/src/never.rs")] {
        let refused = job(&a, &mut s, &l3, &[Edit::delete(gone)], Scope::Affected);
        assert!(
            matches!(refused, Err(CheckError::Edit { .. })),
            "{refused:?}"
        );
    }
    // L4 writes both again: a path a layer below deleted is revived, not refused.
    let l4 = ok(
        &a,
        &mut s,
        &l3,
        &[
            Edit::write(m.clone(), "pub fn m4() {}\n"),
            Edit::write(x.clone(), "pub fn x4() {}\n"),
        ],
        Scope::Workspace,
    );
    assert_eq!(
        text_at(&a, &s, &l4.layer, &m).as_deref(),
        Some("pub fn m4() {}\n")
    );
    assert_eq!(
        text_at(&a, &s, &l4.layer, &x).as_deref(),
        Some("pub fn x4() {}\n")
    );
    assert!(l4.findings.files.contains(&m) && l4.findings.files.contains(&x));
    // L5 deletes the revived file once more.
    let l5 = ok(
        &a,
        &mut s,
        &l4.layer,
        &[Edit::delete(m.clone())],
        Scope::Affected,
    )
    .layer;
    assert_eq!(text_at(&a, &s, &l5, &m), None);

    // The parent and every Layer below keep their texts.
    assert_eq!(
        text_at(&a, &s, &l4.layer, &m).as_deref(),
        Some("pub fn m4() {}\n")
    );
    assert_eq!(text_at(&a, &s, &l3, &m), None);
    assert_eq!(text_at(&a, &s, &l3, &x), None);
    assert_eq!(
        text_at(&a, &s, &l2, &m).as_deref(),
        Some("pub fn m2() {}\n")
    );
    assert_eq!(text_at(&a, &s, &l2, &x).as_deref(), Some(A_X));
    assert_eq!(
        text_at(&a, &s, &l1, &m).as_deref(),
        Some("pub fn m1() {}\n")
    );
    assert_eq!(text_at(&a, &s, &root, &m), None);
    assert_eq!(
        text_at(&a, &s, &root, &rel("a/src/lib.rs")).as_deref(),
        Some(A_LIB)
    );
    drop((root, l1, l2, l3, l4, l5));
    a.close(&mut s).expect("close");
    assert_eq!(a.text(&s, &m), None);
}

#[test]
fn affected_scope_is_the_jobs() {
    let a = analyzer();
    let (_f, mut s) = loaded_w("jobs_affected", &a);
    let root = a.open(&mut s).expect("open");
    let l1 = ok(
        &a,
        &mut s,
        &root,
        &[
            w_edit("c/src/lib.rs", &err_fn("l1")),
            Edit::write(rel("c/src/extra.rs"), "pub fn extra() {}\n"),
        ],
        Scope::Affected,
    );
    assert_eq!(
        l1.findings.files,
        vec![
            rel("c/src/extra.rs"),
            rel("c/src/lib.rs"),
            rel("c/src/z.rs")
        ]
    );

    let l2 = ok(
        &a,
        &mut s,
        &l1.layer,
        &[
            w_edit("a/src/lib.rs", &err_fn("l2")),
            Edit::write(rel("a/src/n.rs"), "pub fn n() {}\n"),
            Edit::delete(rel("a/src/x.rs")),
        ],
        Scope::Affected,
    );
    // a's live files and b's: the created one in, the deleted one out, none of c.
    assert_eq!(
        l2.findings.files,
        vec![
            rel("a/src/lib.rs"),
            rel("a/src/n.rs"),
            rel("b/src/lib.rs"),
            rel("b/src/y.rs")
        ]
    );
    let workspace = workspace_of(&a, &mut s, &l2.layer);
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
    drop((root, l1, l2));
    a.close(&mut s).expect("close");
}

#[test]
fn files_outside_a_jobs_scope_keep_its_parents_findings() {
    let a = analyzer();
    let (_f, mut s) = loaded_w("jobs_outside_scope", &a);
    let root = a.open(&mut s).expect("open");
    // L1 also leaves a macro call in a file of a unexpanded, so a class is observed there.
    let l1 = ok(
        &a,
        &mut s,
        &root,
        &[
            w_edit("a/src/lib.rs", &err_fn("l1")),
            w_edit("a/src/x.rs", "not_a_macro!();\n"),
        ],
        Scope::Affected,
    )
    .layer;
    let at_l1 = workspace_of(&a, &mut s, &l1);
    let l2 = ok(
        &a,
        &mut s,
        &l1,
        &[w_edit("c/src/lib.rs", &err_fn("l2"))],
        Scope::Affected,
    )
    .layer;
    let at_l2 = workspace_of(&a, &mut s, &l2);

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
    drop((root, l1, l2));
    a.close(&mut s).expect("close");
}

#[test]
fn an_empty_job_diagnoses_nothing() {
    let a = analyzer();
    let (_f, mut s) = loaded_w("jobs_empty", &a);
    let root = a.open(&mut s).expect("open");
    let empty = ok(&a, &mut s, &root, &[], Scope::Affected);
    assert!(
        empty.findings.files.is_empty(),
        "{:?}",
        empty.findings.files
    );
    assert!(
        empty.findings.unseen().is_empty(),
        "{:?}",
        empty.findings.unseen()
    );
    assert!(empty.findings.findings.is_empty());
    let workspace = workspace_of(&a, &mut s, &empty.layer);
    assert_eq!(workspace.files, w_paths_sorted());
    for path in w_paths() {
        assert_eq!(
            text_at(&a, &s, &empty.layer, &path).as_deref(),
            Some(base_of(path.as_str())),
            "{path}"
        );
    }
    drop((root, empty));
    a.close(&mut s).expect("close");
}

#[test]
fn a_cut_off_job_keeps_its_parent_usable() {
    let cut = analyzer().with_check_timeout(Some(0));
    let (_f, mut s) = loaded_w("jobs_cut_off", &cut);
    let text = format!("{A_LIB}{}", err_fn("l1"));
    let edits = [Edit::write(rel("a/src/lib.rs"), text.clone())];

    let root = cut.open(&mut s).expect("open");
    let cut_off = job(&cut, &mut s, &root, &edits, Scope::Workspace);
    assert_eq!(
        cut_off.map(|c| c.findings),
        Err(CheckError::TimedOut { limit_ms: 0 })
    );
    // The parent is as it was.
    assert_eq!(
        text_at(&cut, &s, &root, &rel("a/src/lib.rs")).as_deref(),
        Some(A_LIB)
    );
    // And usable: the same job with no cut-off runs on it, and a child on that.
    let found = job_with(&cut, &mut s, &root, &edits, Scope::Workspace, Limit::None)
        .expect("a job with no cut-off");
    assert_eq!(found.findings.files, w_paths_sorted());
    assert_eq!(
        text_at(&cut, &s, &found.layer, &rel("a/src/lib.rs")),
        Some(text)
    );
    let child = job_with(
        &cut,
        &mut s,
        &found.layer,
        &[],
        Scope::Workspace,
        Limit::None,
    )
    .expect("a child of the layer");
    assert_eq!(child.findings.files, w_paths_sorted());
    drop((root, found, child));
    cut.close(&mut s).expect("close");
    assert_eq!(cut.text(&s, &rel("a/src/lib.rs")).as_deref(), Some(A_LIB));
}

#[test]
fn a_job_names_the_unseen_it_observes() {
    let a = analyzer();
    let (_f, mut s) = loaded_w("jobs_unseen", &a);
    let root = a.open(&mut s).expect("open");
    let l1 = ok(
        &a,
        &mut s,
        &root,
        &[w_edit("c/src/lib.rs", &err_fn("l1"))],
        Scope::Affected,
    );
    assert!(
        l1.findings.unseen().is_empty(),
        "{:?}",
        l1.findings.unseen()
    );

    let l2 = ok(
        &a,
        &mut s,
        &l1.layer,
        &[w_edit("a/src/x.rs", "not_a_macro!();\n")],
        Scope::Affected,
    );
    assert_eq!(
        l2.findings.unseen(),
        BTreeSet::from([UnseenClass::UnexpandedMacro])
    );

    // A job above that does not observe it does not name it.
    let l3 = ok(
        &a,
        &mut s,
        &l2.layer,
        &[w_edit("c/src/z.rs", &err_fn("l3"))],
        Scope::Affected,
    );
    assert!(
        l3.findings.unseen().is_empty(),
        "{:?}",
        l3.findings.unseen()
    );
    // A sibling of L2 on L1 observes nothing either.
    let sibling = ok(
        &a,
        &mut s,
        &l1.layer,
        &[w_edit("c/src/z.rs", &err_fn("l3"))],
        Scope::Affected,
    );
    assert!(sibling.findings.unseen().is_empty());
    drop((root, l1, l2, l3, sibling));
    a.close(&mut s).expect("close");
}

#[test]
fn a_baseline_detector_panic_is_named_at_every_job() {
    let a = analyzer().with_detector_fault(Some((GapDetector::UnresolvedPath, rel("a/src/x.rs"))));
    let (_f, mut s) = loaded_w("jobs_baseline_panic", &a);
    let mut parent = a.open(&mut s).expect("open");
    for tag in ["l1", "l2"] {
        let found = job(
            &a,
            &mut s,
            &parent,
            &[w_edit("a/src/lib.rs", &err_fn(tag))],
            Scope::Affected,
        )
        .unwrap_or_else(|e| panic!("{tag}: a baseline panic fails the job: {e:?}"));
        assert!(
            found
                .findings
                .unseen()
                .contains(&UnseenClass::UnresolvedPath),
            "{tag}: {:?}",
            found.findings.unseen()
        );
        parent = found.layer;
    }
    drop(parent);
    a.close(&mut s).expect("close");
}

#[test]
fn a_new_detector_panic_fails_the_job_and_keeps_its_parent() {
    let a = analyzer().with_detector_fault(Some((GapDetector::UnresolvedPath, rel("a/src/m.rs"))));
    let f = w_fixture("jobs_new_panic");
    let mut s = Analyzer::load(&a, &f.dir).expect("load");
    let mut twin = Analyzer::load(&a, &f.dir).expect("load the flat twin");
    let m = rel("a/src/m.rs");
    let panicked = |r: &Result<ScopedFindings, CheckError>| {
        matches!(r, Err(CheckError::DetectorPanicked { detector, file })
            if file == &m && detector == GapDetector::UnresolvedPath.name())
    };

    let l1 = vec![Edit::write(m.clone(), "pub fn m() {}\n")];
    let l2 = vec![w_edit("a/src/lib.rs", &err_fn("l2"))];
    let root = a.open(&mut s).expect("open");
    let at_l1 = job(&a, &mut s, &root, &l1, Scope::Affected).map(|c| c.findings);
    assert!(panicked(&at_l1), "{at_l1:?}");
    assert_eq!(text_at(&a, &s, &root, &m), None);

    // The parent is usable: a job that does not touch the file runs on it.
    let at_l2 = job(&a, &mut s, &root, &l2, Scope::Affected);
    assert!(at_l2.is_ok(), "{:?}", at_l2.map(|c| c.findings));
    // Written on the parent as one job, the two fail as the flat check does.
    let both = compose(&[l1.clone(), l2.clone()]);
    let composed = job(&a, &mut s, &root, &both, Scope::Affected).map(|c| c.findings);
    assert!(panicked(&composed), "{composed:?}");
    assert_eq!(composed, a.check(&mut twin, &both, Scope::Affected));

    drop((root, at_l2));
    a.close(&mut s).expect("close");
    let again = a.open(&mut s).expect("a new session of forks works");
    drop(again);
    a.close(&mut s).expect("close");
    assert_eq!(a.text(&s, &m), None);
}

#[test]
fn a_baseline_panic_brought_back_by_a_job_is_named() {
    let a = analyzer().with_detector_fault(Some((GapDetector::UnresolvedPath, rel("a/src/x.rs"))));
    let f = w_fixture("jobs_panic_back");
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

    let root = a.open(&mut s).expect("open");
    let at_l1 = ok(&a, &mut s, &root, &l1, Scope::Affected);
    assert!(
        !at_l1
            .findings
            .unseen()
            .contains(&UnseenClass::UnresolvedPath),
        "the faulted file is not there: {:?}",
        at_l1.findings.unseen()
    );
    let at_l2 = job(&a, &mut s, &at_l1.layer, &l2, Scope::Affected)
        .expect("a baseline panic brought back is named, not failed");
    assert!(
        at_l2
            .findings
            .unseen()
            .contains(&UnseenClass::UnresolvedPath),
        "{:?}",
        at_l2.findings.unseen()
    );
    let flat = a.check(&mut twin, &compose(&[l1, l2]), Scope::Affected);
    assert_eq!(Ok(at_l2.findings.clone()), flat);
    drop((root, at_l1, at_l2));
    a.close(&mut s).expect("close");
}

/// A name, edits, a cut-off and a detector fault.
type Scenario<'a> = (
    &'a str,
    Vec<Edit>,
    Option<u64>,
    Option<(GapDetector, &'a str)>,
);

fn w_analyzer(timeout: Option<u64>, fault: Option<(GapDetector, &str)>) -> RaAnalyzer {
    analyzer()
        .with_check_timeout(timeout)
        .with_detector_fault(fault.map(|(d, p)| (d, rel(p))))
}

/// `Forks`' side of C12: `open`, one job of `edits` on the root with `Limit::Check`, its `Done`,
/// and `close`.
fn one_job_session(
    a: &RaAnalyzer,
    s: &mut RaSession,
    edits: &[Edit],
    scope: Scope,
) -> Result<ScopedFindings, CheckError> {
    let root = a.open(s)?;
    let done = job(a, s, &root, edits, scope).map(|c| c.findings);
    drop(root);
    a.close(s)?;
    done
}

#[test]
fn check_is_a_one_job_session() {
    let f = w_fixture("jobs_one_job");
    let healthy = w_analyzer(None, None);
    let mut checked = Analyzer::load(&healthy, &f.dir).expect("load");
    let mut forked = Analyzer::load(&healthy, &f.dir).expect("load");
    let m = "a/src/m.rs";
    let plain = vec![w_edit("a/src/lib.rs", &err_fn("p"))];
    let new_file = vec![Edit::write(rel(m), "pub fn m() {}\n")];

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
        let a1 = w_analyzer(timeout, fault);
        let a2 = w_analyzer(timeout, fault);
        let one = a1.check(&mut checked, &edits, Scope::Affected);
        let two = one_job_session(&a2, &mut forked, &edits, Scope::Affected);
        assert_eq!(one, two, "{name}: result");
        for e in &edits {
            assert_eq!(
                a1.text(&checked, &e.path),
                a2.text(&forked, &e.path),
                "{name}: text of {}",
                e.path
            );
        }
    }

    // A detector that panicked at the baseline too: loaded with the fault.
    let fault = Some((GapDetector::UnresolvedPath, "a/src/x.rs"));
    let a1 = w_analyzer(None, fault);
    let a2 = w_analyzer(None, fault);
    let mut checked = Analyzer::load(&a1, &f.dir).expect("load");
    let mut forked = Analyzer::load(&a2, &f.dir).expect("load");
    let edits = vec![w_edit("a/src/lib.rs", &err_fn("p"))];
    let one = a1.check(&mut checked, &edits, Scope::Affected);
    let two = one_job_session(&a2, &mut forked, &edits, Scope::Affected);
    assert_eq!(one, two, "a baseline detector panic: result");
    let named = one.expect("a baseline panic is named, not failed");
    assert!(named.unseen().contains(&UnseenClass::UnresolvedPath));
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
    let a = analyzer();
    let f = w_fixture("jobs_unseen_at_macro");
    let mut s = Analyzer::load(&a, &f.dir).expect("load");
    let mut twin = Analyzer::load(&a, &f.dir).expect("load the twin");
    let edits = vec![w_edit("a/src/x.rs", "not_a_macro!();\n")];
    let root = a.open(&mut s).expect("open");
    for scope in [Scope::Affected, Scope::Workspace] {
        let found = ok(&a, &mut s, &root, &edits, scope).findings;
        assert_eq!(
            found.unseen_at,
            BTreeMap::from([(UnseenClass::UnexpandedMacro, BTreeSet::from([x.clone()]))]),
            "macro, {scope:?}"
        );
        assert_classes_name_their_files(&found, "macro, job");
        let checked = a.check(&mut twin, &edits, scope).expect("check");
        assert_eq!(
            checked.unseen_at, found.unseen_at,
            "macro, check, {scope:?}"
        );
        assert_classes_name_their_files(&checked, "macro, check");
    }
    drop(root);
    a.close(&mut s).expect("close");

    // A detector that panicked on a file at the baseline: named with that file, in a job that
    // edits another file of the crate.
    let b = analyzer().with_detector_fault(Some((GapDetector::UnresolvedPath, x.clone())));
    let g = w_fixture("jobs_unseen_at_panic");
    let mut s = Analyzer::load(&b, &g.dir).expect("load");
    let mut twin = Analyzer::load(&b, &g.dir).expect("load the twin");
    let edits = vec![w_edit("a/src/lib.rs", &err_fn("l1"))];
    let root = b.open(&mut s).expect("open");
    for scope in [Scope::Affected, Scope::Workspace] {
        let found = ok(&b, &mut s, &root, &edits, scope).findings;
        assert_eq!(
            found.unseen_at,
            BTreeMap::from([(UnseenClass::UnresolvedPath, BTreeSet::from([x.clone()]))]),
            "panic, {scope:?}"
        );
        assert_classes_name_their_files(&found, "panic, job");
        let checked = b.check(&mut twin, &edits, scope).expect("check");
        assert_eq!(
            checked.unseen_at, found.unseen_at,
            "panic, check, {scope:?}"
        );
        assert_classes_name_their_files(&checked, "panic, check");
    }
    drop(root);
    b.close(&mut s).expect("close");
}

#[test]
fn layer_files_and_of_level_equal_the_check_over_a_chain() {
    let a = analyzer();
    let f = w_fixture("jobs_layer_files");
    let mut s = Analyzer::load(&a, &f.dir).expect("load the forked session");
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
        let root = a.open(&mut s).expect("open");
        // The root: the files of a check with no edit (none at Affected).
        let none = a
            .layer_files(&s, &root, scope)
            .expect("layer_files of the root");
        let flat = a
            .check(&mut twin, &[], scope)
            .expect("a check with no edit");
        assert_eq!(none, flat.files, "the root, {scope:?}");
        if matches!(scope, Scope::Affected) {
            assert!(none.is_empty(), "{none:?}");
        }

        for (n, levels) in all.iter().enumerate() {
            let mut parent = root.clone();
            let mut below = ScopedFindings::default();
            for (k, edits) in levels.iter().enumerate() {
                let at = format!("sequence {n}, level {}, {scope:?}", k + 1);
                let Checked {
                    layer,
                    findings: level,
                } = job(&a, &mut s, &parent, edits, scope)
                    .unwrap_or_else(|e| panic!("{at}: {e:?}"));
                let files = a.layer_files(&s, &layer, scope).expect("layer_files");
                let flat_edits = net(&levels[..=k]);
                let flat = a
                    .check(&mut twin, &flat_edits, scope)
                    .unwrap_or_else(|e| panic!("{at}: the flat check: {e:?}"));
                assert_eq!(files, flat.files, "{at}: layer_files");
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
                parent = layer;
            }
            drop(parent);
        }
        drop(root);
        a.close(&mut s).expect("close");
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
    // removing it from lib.rs gives an error rust-analyzer reports in part.rs, a file no job
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

    let a = analyzer();
    let f = fixture("jobs_finding_on_its_files");
    let mut s = Analyzer::load(&a, &f.dir).expect("load the forked session");
    let mut twin = Analyzer::load(&a, &f.dir).expect("load the flat twin");
    let baseline = Verifier::baseline(&a, &s);
    assert!(
        baseline.iter().all(|b| b.path != part),
        "no error in part.rs at the base: {baseline:?}"
    );

    for scope in [Scope::Affected, Scope::Workspace] {
        let mut parent = a.open(&mut s).expect("open");
        let mut below = ScopedFindings::default();
        for (k, edits) in levels.iter().enumerate() {
            let at = format!("level {}, {scope:?}", k + 1);
            let Checked {
                layer,
                findings: level,
            } = job(&a, &mut s, &parent, edits, scope).unwrap_or_else(|e| panic!("{at}: {e:?}"));
            assert_findings_on_their_files(&level, &format!("{at}, job"));
            let flat = a
                .check(&mut twin, &flat_edits[k], scope)
                .unwrap_or_else(|e| panic!("{at}: the flat check: {e:?}"));
            assert_findings_on_their_files(&flat, &format!("{at}, check"));

            match k {
                // The error of the edit of lib.rs is on part.rs, in the job and in the check.
                0 => {
                    assert!(on_part(&level), "{at}: {:?}", level.findings);
                    assert!(on_part(&flat), "{at}: {:?}", flat.findings);
                }
                // Crate a is outside the job's scope at Affected: its error is the parent's.
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

            let files = a.layer_files(&s, &layer, scope).expect("layer_files");
            let (verdict, found) = Verdict::of_level(&baseline, &below, &level, &[], &files);
            assert_eq!(found, flat, "{at}: findings");
            assert_eq!(
                verdict,
                Verdict::of_check(&baseline, &flat),
                "{at}: verdict"
            );
            assert_eq!(on_part(&found), k < 2, "{at}: the composed findings");
            below = found;
            parent = layer;
        }
        drop(parent);
        a.close(&mut s).expect("close");
    }
}

#[test]
fn a_failed_base_read_is_a_session_error() {
    let z = rel("c/src/z.rs");
    let a = analyzer().with_db_read_fault(Some(z.clone()));
    let (_f, mut s) = loaded_w("jobs_failed_base_read", &a);

    // L1 changes z, L2 writes its base text back: whether z is an edit of the state needs its
    // base text.
    let root = a.open(&mut s).expect("open");
    let l1 = ok(
        &a,
        &mut s,
        &root,
        &[w_edit("c/src/z.rs", &err_fn("z1"))],
        Scope::Affected,
    )
    .layer;
    let l2 = ok(
        &a,
        &mut s,
        &l1,
        &[Edit::write(z.clone(), C_Z)],
        Scope::Affected,
    )
    .layer;
    let failed = a.layer_files(&s, &l2, Scope::Affected);
    assert!(
        matches!(&failed, Err(e @ CheckError::Load(_)) if e.reach() == CheckReach::Session),
        "{failed:?}"
    );

    // Not poisoned: the Layers and their texts are as before, and the session is used and ends.
    let texts_of = |a: &RaAnalyzer, s: &RaSession, layer: &RaLayer| -> Vec<Option<String>> {
        w_paths().iter().map(|p| text_at(a, s, layer, p)).collect()
    };
    let before = texts_of(&a, &s, &l2);
    assert_eq!(text_at(&a, &s, &l2, &z).as_deref(), Some(C_Z));
    assert_eq!(
        text_at(&a, &s, &l1, &z),
        Some(format!("{C_Z}{}", err_fn("z1")))
    );
    ok(&a, &mut s, &l2, &[], Scope::Affected);
    assert_eq!(texts_of(&a, &s, &l2), before, "a text changed");
    drop((root, l1, l2));
    a.close(&mut s).expect("close");

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

#[test]
fn verifier_reads_describe_the_base_while_forks_are_open() {
    let a = analyzer();
    let (_f, mut s) = loaded_w("jobs_reads", &a);
    let provenance = a.provenance(&s);
    let baseline = Verifier::baseline(&a, &s);
    let root = a.open(&mut s).expect("open");
    let l1 = ok(
        &a,
        &mut s,
        &root,
        &[
            w_edit("c/src/lib.rs", &err_fn("l1")),
            Edit::write(rel("c/src/n.rs"), "pub fn n() {}\n"),
        ],
        Scope::Affected,
    )
    .layer;
    let l2 = ok(
        &a,
        &mut s,
        &l1,
        &[
            Edit::delete(rel("a/src/x.rs")),
            w_edit("a/src/lib.rs", &err_fn("l2")),
        ],
        Scope::Affected,
    )
    .layer;

    for (path, text) in W_FILES {
        assert_eq!(a.text(&s, &rel(path)).as_deref(), Some(text), "{path}");
    }
    assert_eq!(a.text(&s, &rel("c/src/n.rs")), None);
    assert_eq!(a.provenance(&s), provenance);
    assert_eq!(Verifier::baseline(&a, &s), baseline);
    // The Layer itself reads its state.
    assert_eq!(text_at(&a, &s, &l2, &rel("a/src/x.rs")), None);
    assert!(text_at(&a, &s, &l2, &rel("c/src/n.rs")).is_some());
    drop((root, l1, l2));
    a.close(&mut s).expect("close");
}

fn w_blueprint(
    n: u16,
    command: ryo_v2_core::domain::Command,
    path: &str,
    text: &str,
    splices: Vec<ryo_v2_core::domain::Splice>,
    builds_on: Option<ryo_v2_core::domain::BlueprintId>,
) -> ryo_v2_core::domain::Blueprint {
    use ryo_v2_core::domain::{
        BaseFp, Blueprint, BlueprintId, BlueprintParts, DecisionRecord, ReadRange, SpecRef,
        TextRange,
    };
    let len = u32::try_from(text.len()).expect("a short text");
    Blueprint::new(BlueprintParts {
        id: BlueprintId::parse(&format!("01890a5d-ac96-774b-bcce-b302099a{n:04x}")).expect("id"),
        command,
        splices,
        reads: vec![
            ReadRange::capture(rel(path), text, TextRange::new(0, len).expect("range"))
                .expect("read"),
        ],
        bases: vec![BaseFp::capture(rel(path), text)],
        spec_refs: vec![SpecRef::parse("spec#t").expect("spec ref")],
        record: DecisionRecord::new("test", "why").expect("record"),
        builds_on,
    })
    .expect("a Blueprint")
}

#[test]
fn search_and_apply_give_the_same_verdict_on_a_chain() {
    use ryo_v2_core::app::apply::apply_blueprints;
    use ryo_v2_core::app::memory::MemoryBudget;
    use ryo_v2_core::app::search::{DEFAULT_SEARCH_BUDGET, SearchInput, search_decision};
    use ryo_v2_core::domain::{
        BlueprintSet, Command, DEFAULT_WEIGHTS, Ident, Resolution, Splice, TextRange,
    };
    use ryo_v2_core::infra::judge::MetricJudge;
    use ryo_v2_core::infra::memory::ProcGauge;
    use ryo_v2_ra::SyntaxAssessor;

    let a = analyzer();
    let (_f, mut s) = loaded_w("jobs_search_and_apply", &a);
    let range = |at: usize, len: usize| {
        let at = u32::try_from(at).expect("offset");
        TextRange::new(at, at + u32::try_from(len).expect("len")).expect("range")
    };
    // P renames the parameter `s` of `ga` in a/src/lib.rs.
    let lib = "a/src/lib.rs";
    let param = A_LIB.find("ga(s").expect("the parameter") + 3;
    let call = A_LIB.find("    s.missing").expect("the call") + 4;
    let p_splices = [param, call]
        .into_iter()
        .map(|at| Splice::replace(rel(lib), range(at, 1), "t").expect("splice"))
        .collect();
    let rename = Command::RenameLocal {
        from: Ident::parse("s").expect("ident"),
        to: Ident::parse("t").expect("ident"),
    };
    let p = w_blueprint(1, rename, lib, A_LIB, p_splices, None);
    let p_text = A_LIB
        .replace("ga(s: S)", "ga(t: S)")
        .replace("    s.missing", "    t.missing");
    // C, on P's state, extracts a variable in a/src/x.rs, adding an error of its own and a macro
    // call that does not expand.
    let x = "a/src/x.rs";
    let one = A_X.find("    1").expect("the literal") + 4;
    let c_splices = vec![
        Splice::insert(
            rel(x),
            0,
            "fn helper() {\n    let t = 1u32.nope_c();\n    not_a_macro!();\n}\n",
        )
        .expect("insert"),
        Splice::replace(rel(x), range(one, 1), "t").expect("replace"),
    ];
    let extract = Command::ExtractVariable {
        name: Ident::parse("t").expect("ident"),
    };
    let chained = w_blueprint(2, extract.clone(), x, A_X, c_splices.clone(), Some(p.id()));
    let free = w_blueprint(2, extract, x, A_X, c_splices, None);

    // apply: P, then C built on it, each checked flat from the base.
    let set = BlueprintSet::new(vec![p.clone(), chained]).expect("a set");
    let applied = apply_blueprints(&a, &mut s, &set, Scope::Affected).expect("apply");
    let Resolution::Decided { evidence, .. } = &applied else {
        panic!("apply decides P: {applied:?}");
    };
    let flat: BTreeMap<_, _> = evidence.verdicts().iter().cloned().collect();

    // The search, twice on its forks: P as a root on the base, then C as a root on P's state,
    // the start state's job below C's. A beam of 1 holds the root alone.
    let mut budget = DEFAULT_SEARCH_BUDGET;
    budget.max_depth = 1;
    budget.beam = 1;
    let mut search = |start: &[Edit], root: &ryo_v2_core::domain::Blueprint| {
        let roots = [root.clone()];
        let report = search_decision(
            &a,
            &SyntaxAssessor,
            &MetricJudge,
            &MonotonicClock::new(),
            &ProcGauge,
            &mut s,
            &SearchInput {
                start,
                roots: &roots,
                places: &[],
                probes: &[],
                scope: Scope::Affected,
                budget,
                memory: MemoryBudget::unbounded(),
                weights: DEFAULT_WEIGHTS,
                keep_trace_edits: false,
            },
        )
        .expect("search");
        let verdicts: BTreeMap<_, _> = match &report.resolution {
            Resolution::Decided { evidence, .. } => evidence.verdicts().iter().cloned().collect(),
            Resolution::Failed(i) => i
                .blocking()
                .iter()
                .filter_map(|b| match b {
                    ryo_v2_core::domain::Blocking::NewErrors { blueprint, verdict } => {
                        Some((*blueprint, verdict.clone()))
                    }
                    _ => None,
                })
                .collect(),
            Resolution::Stopped(m) => match *m {},
        };
        verdicts
    };
    let on_base = search(&[], &p);
    let on_p = search(&[Edit::write(rel(lib), p_text)], &free);

    let p_verdict = &on_base[&p.id()];
    assert_eq!(p_verdict, &flat[&p.id()], "P: the same verdict");
    assert!(p_verdict.passed(), "P passes: {p_verdict:?}");
    let c = &on_p[&free.id()];
    assert_eq!(
        c,
        &flat[&free.id()],
        "C on the start's job: the same verdict, unseen classes included"
    );
    assert!(!c.passed(), "C adds errors: {c:?}");
    assert!(
        c.unseen.contains(&UnseenClass::UnexpandedMacro),
        "C's macro call is observed: {c:?}"
    );
}

#[test]
fn a_base_write_with_a_live_layer_is_fork_in_use() {
    let a = analyzer();
    let (_f, mut s) = loaded_w("jobs_base_write_live_layer", &a);
    let root = a.open(&mut s).expect("open");
    let layer = ok(&a, &mut s, &root, &w_mixed(), Scope::Affected).layer;
    drop(root);

    // Every operation that writes the base refuses while a layer of it lives, and waits for
    // nothing.
    let edit = w_edit("a/src/lib.rs", &err_fn("base"));
    assert_eq!(
        a.check(&mut s, std::slice::from_ref(&edit), Scope::Affected),
        Err(CheckError::ForkInUse)
    );
    assert_eq!(
        a.reference_check(&mut s, std::slice::from_ref(&edit), Scope::Affected),
        Err(CheckError::ForkInUse)
    );
    assert_eq!(a.audit_revert(&mut s), Err(CheckError::ForkInUse));
    assert_eq!(s.memory_report().err(), Some(CheckError::ForkInUse));
    assert!(matches!(
        Analyzer::fork(&a, &mut s),
        Err(AnalyzerError::ForkInUse)
    ));
    assert!(matches!(a.open(&mut s), Err(CheckError::ForkInUse)));
    // Nothing changed: the base reads as at load, the Layer as it was.
    for (path, text) in W_FILES {
        assert_eq!(a.text(&s, &rel(path)).as_deref(), Some(text), "{path}");
    }
    assert!(text_at(&a, &s, &layer, &rel("a/src/n.rs")).is_some());
    assert_eq!(text_at(&a, &s, &layer, &rel("c/src/z.rs")), None);
    drop(layer);
    a.close(&mut s).expect("close");
    assert_eq!(s.fork_counters().map(|c| c.refused), Some(0), "I2");
    a.check(&mut s, &[edit], Scope::Affected)
        .expect("a check once the forks have ended");
}

#[test]
fn a_panic_in_a_layer_poisons_only_that_layer() {
    // A write that panics in its layer, and a diagnosis that panics in it: each job is a `Done`
    // with `Panicked`, of reach Blueprint, and no layer; its parent is as it was and no write of
    // the base was refused.
    let injected = [
        (
            "a write",
            analyzer().with_db_write_panic(Some(DbWritePanic::Write)),
        ),
        ("a diagnosis", analyzer().with_diagnose_panic(true)),
    ];
    for (n, (what, a)) in injected.iter().enumerate() {
        let (_f, mut s) = loaded_w(&format!("jobs_layer_poison_{n}"), a);
        let root = a.open(&mut s).expect("open");
        let failed = job(
            a,
            &mut s,
            &root,
            &[w_edit("c/src/lib.rs", &err_fn("l2"))],
            Scope::Affected,
        );
        assert!(
            matches!(&failed, Err(e @ CheckError::Panicked(_)) if e.reach() == CheckReach::Blueprint),
            "{what}: {:?}",
            failed.map(|c| c.findings)
        );
        for (path, text) in W_FILES {
            assert_eq!(
                text_at(a, &s, &root, &rel(path)).as_deref(),
                Some(text),
                "{what}: {path}"
            );
        }
        drop(root);
        a.close(&mut s).expect("close");
        assert_eq!(s.fork_counters().map(|c| c.refused), Some(0), "{what}: I2");
    }

    // The panic of the second job, in its own code: the layer below, a sibling written after it
    // and the session are untouched.
    let calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = calls.clone();
    let a = analyzer().with_job_hook(Some(Arc::new(move |_| {
        if hook_calls.fetch_add(1, Ordering::SeqCst) == 1 {
            panic!("an injected panic in a job");
        }
    })));
    let (_f, mut s) = loaded_w("jobs_layer_poison_sibling", &a);
    let root = a.open(&mut s).expect("open");
    let l1 = ok(&a, &mut s, &root, &w_mixed(), Scope::Affected).layer;
    let texts = |a: &RaAnalyzer, s: &RaSession| -> Vec<Option<String>> {
        w_paths().iter().map(|p| text_at(a, s, &l1, p)).collect()
    };
    let before = texts(&a, &s);
    let l2 = vec![w_edit("c/src/lib.rs", &err_fn("l2"))];
    let failed = job(&a, &mut s, &l1, &l2, Scope::Affected);
    assert!(
        matches!(&failed, Err(e @ CheckError::Panicked(_)) if e.reach() == CheckReach::Blueprint),
        "{:?}",
        failed.map(|c| c.findings)
    );
    assert_eq!(texts(&a, &s), before, "the layer below changed");

    let l2 = vec![w_edit("b/src/lib.rs", &err_fn("l2b"))];
    let sibling = ok(&a, &mut s, &l1, &l2, Scope::Workspace).findings;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    drop((root, l1));
    a.close(&mut s).expect("close");
    let flat = a
        .reference_check(&mut s, &compose(&[w_mixed(), l2]), Scope::Workspace)
        .expect("the reference check");
    assert_eq!(sibling, flat);
    assert_eq!(s.fork_counters().map(|c| c.refused), Some(0), "I2");
}

#[test]
fn a_jobs_paths_join_the_revert_audit() {
    let a = analyzer();
    let faulty =
        analyzer().with_detector_fault(Some((GapDetector::UnresolvedPath, rel("a/src/lib.rs"))));
    let (_f, mut s) = loaded_w("jobs_audit", &a);
    // The control: nothing was written, so there is nothing to audit.
    assert_eq!(faulty.audit_revert(&mut s), Ok(()));

    let root = a.open(&mut s).expect("open");
    // A refused job is recorded too.
    let never = rel("a/src/never.rs");
    let refused = job(
        &a,
        &mut s,
        &root,
        &[Edit::delete(never.clone())],
        Scope::Affected,
    );
    assert!(
        matches!(refused, Err(CheckError::Edit { .. })),
        "{:?}",
        refused.map(|c| c.findings)
    );
    let done = ok(
        &a,
        &mut s,
        &root,
        &[w_edit("a/src/lib.rs", "\npub fn h() {}\n")],
        Scope::Affected,
    );
    drop((root, done));
    a.close(&mut s).expect("close");
    assert_eq!(s.pending_audit(), vec![rel("a/src/lib.rs"), never]);

    // The audit re-diagnoses what the jobs wrote: the faulty detector differs from the baseline
    // there.
    let audited = faulty.audit_revert(&mut s);
    assert!(
        matches!(audited, Err(CheckError::RevertMismatch { .. })),
        "{audited:?}"
    );
}
