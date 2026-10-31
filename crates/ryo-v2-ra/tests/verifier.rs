//! `Verifier` for `RaAnalyzer`, on throwaway workspaces (the corpus is never touched).

use ryo_v2_core::app::ports::{Analyzer, CheckError, Event, Verifier};
use ryo_v2_core::domain::{Edit, ErrorCode, FindingCode, RelPath, Scope};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_ra::{RaAnalyzer, RaConfig, RaSession};
use std::path::PathBuf;
use std::sync::Arc;

const LIB: &str = "pub struct S;\n\npub fn a(s: &S) -> u32 {\n    s.missing()\n}\n";

/// A temp workspace that removes itself on drop.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(test: &str, files: &[(&str, &str)]) -> Self {
        // Not CARGO_TARGET_TMPDIR: `session::load` skips every path with a `target` component,
        // so a workspace under target/ would have no indexed files.
        let dir = std::env::temp_dir().join(format!("ryo-v2-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (rel, text) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).unwrap();
            std::fs::write(path, text).unwrap();
        }
        Self { dir }
    }

    fn single(test: &str, lib: &str) -> Self {
        Self::new(
            test,
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
                ),
                ("src/lib.rs", lib),
            ],
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn load(
    f: &Fixture,
    every: Option<u32>,
    cfg_test: bool,
) -> (RaAnalyzer, RaSession, Arc<RecordingObserver>) {
    let observer = Arc::new(RecordingObserver::default());
    let a = RaAnalyzer::new(
        RaConfig {
            proc_macro: false,
            threads: Some(1),
            assert_revert: false,
            unresolved_paths: true,
            assert_revert_every: every.and_then(std::num::NonZeroU32::new),
            cfg_test,
        },
        Arc::new(MonotonicClock::new()),
        observer.clone(),
    );
    let s = a.load(&f.dir).expect("load fixture");
    (a, s, observer)
}

fn rel(s: &str) -> RelPath {
    RelPath::parse(s).unwrap()
}

fn lib_edit(extra: &str) -> Edit {
    Edit::write(rel("src/lib.rs"), format!("{LIB}{extra}"))
}

#[test]
fn check_leaves_text_and_baseline_at_base() {
    let f = Fixture::single("verifier_leaves", LIB);
    let (a, mut s, _) = load(&f, None, false);
    let base_findings = Verifier::baseline(&a, &s);
    assert_eq!(base_findings.len(), 1, "got {base_findings:?}");
    assert_eq!(a.text(&s, &rel("src/lib.rs")).as_deref(), Some(LIB));
    assert_eq!(a.text(&s, &rel("Cargo.toml")), None);

    let edit = lib_edit("\npub fn b(s: &S) -> u32 {\n    s.missing()\n}\n");
    let r = a
        .check(&mut s, std::slice::from_ref(&edit), Scope::Affected)
        .expect("check");
    let e0599 = FindingCode::Rustc(ErrorCode::parse("E0599").unwrap());
    assert_eq!(r.findings.iter().filter(|x| x.code == e0599).count(), 2);

    assert_eq!(a.text(&s, &rel("src/lib.rs")).as_deref(), Some(LIB));
    assert_eq!(Verifier::baseline(&a, &s), base_findings);
    let again = a
        .check(&mut s, &[edit], Scope::Affected)
        .expect("second check");
    assert_eq!(again, r);
}

#[test]
fn facts_mark_build_script_and_proc_macro_crate() {
    let f = Fixture::new(
        "verifier_facts",
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"root\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n\n[workspace]\nmembers = [\"pm\"]\n",
            ),
            ("build.rs", "fn main() {}\n"),
            ("src/lib.rs", "pub fn f() {}\n"),
            (
                "pm/Cargo.toml",
                "[package]\nname = \"pm\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n",
            ),
            ("pm/src/lib.rs", "pub fn g() {}\n"),
        ],
    );
    let (a, s, _) = load(&f, None, false);
    let bs = a.facts(&s, &rel("build.rs"));
    assert!(bs.is_build_script && !bs.in_proc_macro_crate, "{bs:?}");
    let pm = a.facts(&s, &rel("pm/src/lib.rs"));
    assert!(pm.in_proc_macro_crate && !pm.is_build_script, "{pm:?}");
    let plain = a.facts(&s, &rel("src/lib.rs"));
    assert!(
        !plain.in_proc_macro_crate && !plain.is_build_script,
        "{plain:?}"
    );
}

#[test]
fn assert_revert_every_samples_the_diagnostic_assert() {
    let f = Fixture::single("verifier_every", LIB);
    let (a, mut s, obs) = load(&f, Some(2), false);
    let edit = lib_edit("\npub fn b() {}\n");
    let mut revert_oks = Vec::new();
    for _ in 0..2 {
        obs.take();
        a.check(&mut s, std::slice::from_ref(&edit), Scope::Affected)
            .expect("check");
        let ok = obs.take().into_iter().find_map(|e| match e {
            Event::Reverted { revert_ok, .. } => Some(revert_ok),
            _ => None,
        });
        revert_oks.push(ok);
    }
    assert_eq!(revert_oks, vec![Some(None), Some(Some(true))]);
}

#[test]
fn check_of_unindexed_existing_path_is_edit_error() {
    let f = Fixture::single("verifier_manifest", LIB);
    let (a, mut s, _) = load(&f, None, false);
    let edit = Edit::write(rel("Cargo.toml"), "[package]\n");
    let err = a
        .check(&mut s, &[edit], Scope::Affected)
        .expect_err("a manifest is not indexed");
    assert!(matches!(err, CheckError::Edit { .. }), "{err:?}");
    // The session is still usable.
    a.check(&mut s, &[lib_edit("\npub fn c() {}\n")], Scope::Affected)
        .expect("check after the rejected edit");
}

#[test]
fn cfg_test_config_diagnoses_test_module() {
    let lib =
        "pub fn f() {}\n\n#[cfg(test)]\nmod t {\n    fn g() {\n        missing_fn();\n    }\n}\n";
    let e0425 = FindingCode::Rustc(ErrorCode::parse("E0425").unwrap());
    let has_e0425 = |cfg_test: bool| {
        let f = Fixture::single(&format!("verifier_cfg_{cfg_test}"), lib);
        let (a, s, _) = load(&f, None, cfg_test);
        Verifier::baseline(&a, &s)
            .iter()
            .any(|x| x.code == e0425 && x.line == 6)
    };
    assert!(
        !has_e0425(false),
        "cfg(test) code is cfg-disabled by default"
    );
    assert!(has_e0425(true), "set_test enables cfg(test) code");
}
