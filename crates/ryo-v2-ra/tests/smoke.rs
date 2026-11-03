//! Smoke: corpus fixtures through the real rust-analyzer. Needs a sysroot and
//! `cargo metadata` (no network).

use ryo_v2_core::app::ports::{Analyzer, Event};
use ryo_v2_core::app::verify::verify;
use ryo_v2_core::domain::{Edit, FindingCode, RelPath, Scope, Verdict};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_ra::{RaAnalyzer, RaConfig, RaSession};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn corpus(rel: &str) -> PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../corpus"))
        .join(rel)
        .canonicalize()
        .expect("fixture exists")
}

fn corpus_edit(name: &str, rel: &str) -> Edit {
    let text = std::fs::read_to_string(corpus(name).join(rel)).expect("corpus file");
    Edit::write(RelPath::parse(rel).unwrap(), text)
}

fn analyzer() -> (RaAnalyzer, Arc<RecordingObserver>) {
    let observer = Arc::new(RecordingObserver::default());
    let a = RaAnalyzer::new(
        RaConfig {
            assert_revert: true,
            // The fixtures' test modules still use the names their edits rename.
            cfg_test: false,
            ..RaConfig::default()
        },
        Arc::new(MonotonicClock::new()),
        observer.clone(),
    );
    (a, observer)
}

/// Verifies, and asserts that the revert restored the baseline.
fn check(a: &RaAnalyzer, obs: &RecordingObserver, s: &mut RaSession, edits: &[Edit]) -> Verdict {
    obs.take();
    let v = verify(a, s, edits, Scope::Affected).expect("verify (a RevertMismatch fails here)");
    let revert_ok = obs.take().into_iter().find_map(|e| match e {
        Event::Reverted { revert_ok, .. } => Some(revert_ok),
        _ => None,
    });
    assert_eq!(revert_ok, Some(Some(true)), "revert must restore baseline");
    v
}

fn rel(s: &str) -> RelPath {
    RelPath::parse(s).unwrap()
}

#[test]
fn indexed_comes_before_loaded() {
    let (a, obs) = analyzer();
    a.load(&corpus("base/extract-trait-multicrate"))
        .expect("load fixture");
    let events = obs.take();
    let indexed: Vec<(usize, u64, usize)> = events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match e {
            Event::Indexed { load_ms, files } => Some((i, *load_ms, *files)),
            _ => None,
        })
        .collect();
    let loaded: Vec<(usize, u64, usize)> = events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match e {
            Event::Loaded { load_ms, files, .. } => Some((i, *load_ms, *files)),
            _ => None,
        })
        .collect();
    assert_eq!(indexed.len(), 1, "{events:?}");
    assert_eq!(loaded.len(), 1, "{events:?}");
    let ((at_indexed, load_ms, files), (at_loaded, loaded_ms, loaded_files)) =
        (indexed[0], loaded[0]);
    assert!(at_indexed < at_loaded, "{events:?}");
    assert_eq!((load_ms, files), (loaded_ms, loaded_files));
    assert!(files > 0);
}

#[test]
fn broken01_detected_and_valid_passes() {
    let (a, obs) = analyzer();
    let mut s = a
        .load(&corpus("base/extract-trait-multicrate"))
        .expect("load fixture");
    assert!(a.baseline(&s).is_empty(), "fixture baseline must be clean");

    // broken-01: lib 側 Case01::one() 削除 → user 側 cross-crate caller が E0599
    let v = check(
        &a,
        &obs,
        &mut s,
        &[corpus_edit("broken-01", "lib/src/lib.rs")],
    );
    assert!(!v.passed(), "broken-01 must FAIL");
    assert!(
        v.new_errors
            .errors
            .iter()
            .any(|d| d.code.as_str() == "E0599"),
        "expected E0599, got {:?}",
        v.new_errors
    );

    // valid-02: 完遂 rename (lib + caller 両更新) → PASS
    let v = check(
        &a,
        &obs,
        &mut s,
        &[
            corpus_edit("valid-02", "lib/src/lib.rs"),
            corpus_edit("valid-02", "user/src/lib.rs"),
        ],
    );
    assert!(v.passed(), "valid-02 must PASS, got {:?}", v.new_errors);

    // Created (新規 module file) — valid: 親に mod 宣言 + 新 file、PASS 期待
    let base_lib =
        std::fs::read_to_string(corpus("base/extract-trait-multicrate").join("lib/src/lib.rs"))
            .unwrap();
    let with_mod = format!("{base_lib}\nmod hircheck_probe_mod;\n");
    let v = check(
        &a,
        &obs,
        &mut s,
        &[
            Edit::write(rel("lib/src/lib.rs"), with_mod.clone()),
            Edit::write(
                rel("lib/src/hircheck_probe_mod.rs"),
                "pub(crate) fn probe(c: &crate::Case01) -> u32 {\n    c.one()\n}\n",
            ),
        ],
    );
    assert!(
        v.passed(),
        "created-valid must PASS, got {:?}",
        v.new_errors
    );

    // Created — broken: 新 file が存在しないメソッドを呼ぶ → FAIL 期待
    let v = check(
        &a,
        &obs,
        &mut s,
        &[
            Edit::write(rel("lib/src/lib.rs"), with_mod),
            Edit::write(
                rel("lib/src/hircheck_probe_mod.rs"),
                "pub(crate) fn broken(c: &crate::Case01) -> u32 {\n    c.nonexistent_method()\n}\n",
            ),
        ],
    );
    assert!(!v.passed(), "created-broken must FAIL");
    assert!(
        v.new_errors
            .errors
            .iter()
            .any(|d| d.path.as_str() == "lib/src/hircheck_probe_mod.rs"),
        "expected error in the created file, got {:?}",
        v.new_errors
    );

    // Created 後の通常 check が無事なこと (状態リーク検査)
    let v = check(
        &a,
        &obs,
        &mut s,
        &[corpus_edit("broken-01", "lib/src/lib.rs")],
    );
    assert!(!v.passed());
}

/// Deleted 軸。base = corpus/created-broken-disk (probe module に E0599 が 1 件 baseline)。
#[test]
fn deleted_file_detection() {
    let base = corpus("created-broken-disk");
    let (a, obs) = analyzer();
    let mut s = a.load(&base).expect("load fixture");
    let baseline = a.baseline(&s);
    assert_eq!(
        baseline
            .iter()
            .filter(|f| matches!(f.code, FindingCode::Rustc(_)))
            .count(),
        1,
        "fixture baseline = probe module の E0599 1 件, got {baseline:?}"
    );

    // 削除のみ (親の `mod hircheck_probe_mod;` が残る) → unresolved module で FAIL
    let v = check(
        &a,
        &obs,
        &mut s,
        &[Edit::delete(rel("lib/src/hircheck_probe_mod.rs"))],
    );
    assert!(!v.passed(), "dangling mod decl must FAIL");
    let at_lib_rs = v
        .new_errors
        .errors
        .iter()
        .map(|d| d.path.as_str())
        .chain(v.new_errors.unmapped.iter().map(|d| d.path.as_str()))
        .any(|p| p == "lib/src/lib.rs");
    assert!(
        at_lib_rs,
        "expected error at the dangling mod decl, got {:?}",
        v.new_errors
    );

    // 削除 + 親から mod 宣言も除去 (完遂形) → PASS
    let base_lib = std::fs::read_to_string(base.join("lib/src/lib.rs")).unwrap();
    let fixed_lib = base_lib.replace("mod hircheck_probe_mod;\n", "");
    assert_ne!(fixed_lib, base_lib, "mod decl must be removed");
    let v = check(
        &a,
        &obs,
        &mut s,
        &[
            Edit::delete(rel("lib/src/hircheck_probe_mod.rs")),
            Edit::write(rel("lib/src/lib.rs"), fixed_lib),
        ],
    );
    assert!(
        v.passed(),
        "completed deletion must PASS, got {:?}",
        v.new_errors
    );

    // 削除 cycle 後の通常 check が無事なこと (状態リーク検査)
    let v = check(
        &a,
        &obs,
        &mut s,
        &[Edit::write(
            rel("lib/src/lib.rs"),
            format!("{base_lib}\npub fn extra_probe() -> u32 {{ 1 }}\n"),
        )],
    );
    assert!(v.passed(), "got {:?}", v.new_errors);
}
