//! `Verifier` for `RaAnalyzer`, on throwaway workspaces (the corpus is never touched).

// For the backend switch of the tests of both backends (`common::both`).
mod common;

use ryo_v2_core::app::ports::{Analyzer, AnalyzerError, CheckError, Event, Verifier};
use ryo_v2_core::domain::{Edit, ErrorCode, FindingCode, Fingerprint, RelPath, Scope};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_ra::{GapDetector, RaAnalyzer, RaConfig, RaSession};
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
            gap_detectors: true,
            assert_revert_every: every.and_then(std::num::NonZeroU32::new),
            cfg_test,
            engine: common::engine(),
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
    common::both(check_leaves_text_and_baseline_at_base_on);
}

fn check_leaves_text_and_baseline_at_base_on() {
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
    common::both(facts_mark_build_script_and_proc_macro_crate_on);
}

fn facts_mark_build_script_and_proc_macro_crate_on() {
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
    common::both(assert_revert_every_samples_the_diagnostic_assert_on);
}

fn assert_revert_every_samples_the_diagnostic_assert_on() {
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
    common::both(check_of_unindexed_existing_path_is_edit_error_on);
}

fn check_of_unindexed_existing_path_is_edit_error_on() {
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
    common::both(cfg_test_config_diagnoses_test_module_on);
}

fn cfg_test_config_diagnoses_test_module_on() {
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
        "cfg(test) code is cfg-disabled with cfg_test off"
    );
    assert!(has_e0425(true), "set_test enables cfg(test) code");
}

#[test]
fn ra_config_default_diagnoses_test_code() {
    assert!(RaConfig::default().cfg_test);
    // The undo log stays the default backend of the forks (D11, stage 3).
    assert!(!RaConfig::default().engine);
}

const UNRESOLVED_LIB: &str =
    "pub struct S;\npub fn g(s: S) {\n    s.missing();\n}\npub fn f(_x: Missing) {}\n";

/// An analyzer with the revert assert on; `fault` makes the detector panic on `src/lib.rs`, so a
/// re-assert under it lacks the detector finding that the baseline has.
fn assert_analyzer(fault: bool) -> RaAnalyzer {
    let a = RaAnalyzer::new(
        RaConfig {
            proc_macro: false,
            threads: Some(1),
            assert_revert: true,
            gap_detectors: true,
            assert_revert_every: None,
            cfg_test: false,
            engine: common::engine(),
        },
        Arc::new(MonotonicClock::new()),
        Arc::new(RecordingObserver::default()),
    );
    a.with_detector_fault(fault.then(|| (GapDetector::UnresolvedPath, rel("src/lib.rs"))))
}

#[test]
fn reassert_mismatch_poisons_the_session() {
    common::both(reassert_mismatch_poisons_the_session_on);
}

fn reassert_mismatch_poisons_the_session_on() {
    let f = Fixture::single("verifier_reassert_poison", UNRESOLVED_LIB);
    let healthy = assert_analyzer(false);
    let faulty = assert_analyzer(true);
    let mut s = healthy.load(&f.dir).expect("load fixture");
    let e0425 = FindingCode::Rustc(ErrorCode::parse("E0425").unwrap());
    assert!(
        Verifier::baseline(&healthy, &s)
            .iter()
            .any(|x| x.code == e0425),
        "the baseline holds the detector finding"
    );
    let edit = Edit::write(
        rel("src/lib.rs"),
        format!("{UNRESOLVED_LIB}pub fn h() {{}}\n"),
    );
    let first = faulty.check(&mut s, std::slice::from_ref(&edit), Scope::Affected);
    if common::engine() {
        // The Engine re-asserts no revert: its sampled pop checks the isolation of the base,
        // which a detector fault does not touch. The new detector panic decides (D7).
        assert!(
            matches!(first, Err(CheckError::DetectorPanicked { .. })),
            "{first:?}"
        );
        healthy
            .check(&mut s, &[edit], Scope::Affected)
            .expect("the session stays usable");
        return;
    }
    assert!(
        matches!(first, Err(CheckError::RevertMismatch { .. })),
        "{first:?}"
    );
    let second = healthy.check(&mut s, &[edit], Scope::Affected);
    assert!(matches!(second, Err(CheckError::Poisoned(_))), "{second:?}");
}

#[test]
fn session_errors_win_over_a_detector_panic() {
    common::both(session_errors_win_over_a_detector_panic_on);
}

fn session_errors_win_over_a_detector_panic_on() {
    let f = Fixture::single("verifier_session_wins", UNRESOLVED_LIB);
    let healthy = assert_analyzer(false);
    let faulty = assert_analyzer(true);
    let edit = Edit::write(
        rel("src/lib.rs"),
        format!("{UNRESOLVED_LIB}pub fn h() {{}}\n"),
    );
    // The detector panics in the check's own diagnostics, and the re-assert after the revert
    // lacks the finding the baseline has: the session error is the result.
    let mut s = healthy.load(&f.dir).expect("load fixture");
    let first = faulty.check(&mut s, std::slice::from_ref(&edit), Scope::Affected);
    if common::engine() {
        // No revert on the Engine, so no session error: the detector panic is the result, and
        // stays so.
        for checked in [
            first,
            faulty.check(&mut s, std::slice::from_ref(&edit), Scope::Affected),
        ] {
            assert!(
                matches!(checked, Err(CheckError::DetectorPanicked { .. })),
                "{checked:?}"
            );
        }
    } else {
        assert!(
            matches!(first, Err(CheckError::RevertMismatch { .. })),
            "{first:?}"
        );
        // A poisoned session fails before the detector runs.
        let second = faulty.check(&mut s, std::slice::from_ref(&edit), Scope::Affected);
        assert!(matches!(second, Err(CheckError::Poisoned(_))), "{second:?}");
    }
    let mut poisoned = healthy.load(&f.dir).expect("load fixture");
    poisoned.poison("x");
    let third = faulty.check(&mut poisoned, &[edit], Scope::Affected);
    assert!(matches!(third, Err(CheckError::Poisoned(_))), "{third:?}");
}

#[test]
fn discard_reassert_mismatch_poisons_the_session() {
    common::both(discard_reassert_mismatch_poisons_the_session_on);
}

fn discard_reassert_mismatch_poisons_the_session_on() {
    let f = Fixture::single("verifier_discard_poison", UNRESOLVED_LIB);
    let healthy = assert_analyzer(false);
    let faulty = assert_analyzer(true);
    let mut s = healthy.load(&f.dir).expect("load fixture");
    let mut fork = Analyzer::fork(&faulty, &mut s).expect("fork");
    let edit = Edit::write(
        rel("src/lib.rs"),
        format!("{UNRESOLVED_LIB}pub fn h() {{}}\n"),
    );
    Analyzer::apply(&faulty, &mut s, &mut fork, &[edit]).expect("apply");
    Analyzer::diagnostics(&faulty, &s, &fork, Scope::Affected).expect("diagnostics");
    let discarded = Analyzer::discard(&faulty, &mut s, fork);
    if common::engine() {
        // The Engine's discard drops the layer and checks the isolation of the base, which the
        // detector fault does not touch.
        discarded.expect("a discard on the Engine reverts nothing");
        let next = Analyzer::fork(&healthy, &mut s).expect("the session stays usable");
        Analyzer::discard(&healthy, &mut s, next).expect("discard");
        return;
    }
    assert!(
        matches!(discarded, Err(AnalyzerError::RevertMismatch)),
        "{discarded:?}"
    );
    let next = Analyzer::fork(&healthy, &mut s);
    assert!(next.is_err(), "a poisoned session must refuse a fork");
}

#[test]
fn provenance_records_the_gap_detector_switch() {
    common::both(provenance_records_the_gap_detector_switch_on);
}

fn provenance_records_the_gap_detector_switch_on() {
    for on in [true, false] {
        let f = Fixture::single(&format!("verifier_prov_gap_{on}"), LIB);
        let a = RaAnalyzer::new(
            RaConfig {
                proc_macro: false,
                threads: Some(1),
                assert_revert: false,
                gap_detectors: on,
                assert_revert_every: None,
                cfg_test: false,
                engine: common::engine(),
            },
            Arc::new(MonotonicClock::new()),
            Arc::new(RecordingObserver::default()),
        );
        let s = a.load(&f.dir).expect("load fixture");
        let p = Verifier::provenance(&a, &s);
        let config = p.config();
        assert_eq!(
            config.get("gap_detectors").map(String::as_str),
            Some(on.to_string().as_str()),
            "got {config:?}"
        );
        assert!(!config.contains_key("unresolved_paths"), "got {config:?}");
        let keys: Vec<&str> = config.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "cfg_test",
                "check_timeout_ms",
                "gap_detectors",
                "proc_macro",
                "toolchain"
            ]
        );
    }
}

fn provenance_of(f: &Fixture) -> ryo_v2_core::domain::Provenance {
    let a = RaAnalyzer::new(
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
    );
    let s = a.load(&f.dir).expect("load fixture");
    Verifier::provenance(&a, &s)
}

#[test]
fn provenance_fingerprints_manifests_lock_and_toolchain() {
    common::both(provenance_fingerprints_manifests_lock_and_toolchain_on);
}

fn provenance_fingerprints_manifests_lock_and_toolchain_on() {
    let f = Fixture::single("verifier_prov_inputs", LIB);
    let p = provenance_of(&f);
    assert_eq!(p.analyzer(), "rust-analyzer");
    assert_eq!(p.version(), "0.0.358");
    let names: Vec<String> = p.files().keys().map(|k| format!("{k:?}")).collect();
    for want in ["src/lib.rs", "Cargo.toml", "Cargo.lock"] {
        assert!(
            p.files().contains_key(&RelPath::parse(want).unwrap()),
            "{want} missing from {names:?}"
        );
    }
    let toolchain = p.config().get("toolchain").expect("toolchain entry");
    assert!(toolchain.starts_with("rustc "), "got {toolchain:?}");

    // Same `.rs` text, different manifest: the workspace fingerprint differs.
    let manifest = |extra: &str| {
        format!(
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n{extra}"
        )
    };
    let a = Fixture::new(
        "verifier_prov_manifest_a",
        &[("Cargo.toml", &manifest("")), ("src/lib.rs", LIB)],
    );
    let b = Fixture::new(
        "verifier_prov_manifest_b",
        &[
            ("Cargo.toml", &manifest("\n[features]\nextra = []\n")),
            ("src/lib.rs", LIB),
        ],
    );
    let (pa, pb) = (provenance_of(&a), provenance_of(&b));
    let lib = RelPath::parse("src/lib.rs").unwrap();
    assert_eq!(pa.files().get(&lib), pb.files().get(&lib));
    assert_ne!(pa.workspace(), pb.workspace());
}

#[test]
fn provenance_fingerprints_are_of_the_disk_text() {
    common::both(provenance_fingerprints_are_of_the_disk_text_on);
}

fn provenance_fingerprints_are_of_the_disk_text_on() {
    // The check before a write compares these with the fingerprint of the file's bytes, so a
    // line ending must not be normalized on the way in.
    let text = "pub fn a() {}\r\npub fn b() {}\r\n// lone\rcarriage return\n";
    let f = Fixture::single("verifier_prov_disk_text", text);
    let p = provenance_of(&f);
    let on_disk = std::fs::read(f.dir.join("src/lib.rs")).unwrap();
    assert_eq!(on_disk, text.as_bytes());
    assert_eq!(
        p.files().get(&rel("src/lib.rs")),
        Some(&Fingerprint::of(&on_disk))
    );
}

#[test]
fn provenance_fingerprints_each_member_manifest() {
    common::both(provenance_fingerprints_each_member_manifest_on);
}

fn provenance_fingerprints_each_member_manifest_on() {
    let f = Fixture::new(
        "verifier_prov_members",
        &[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
            ),
            (
                "a/Cargo.toml",
                "[package]\nname = \"ma\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("a/src/lib.rs", "pub fn a() {}\n"),
            (
                "b/Cargo.toml",
                "[package]\nname = \"mb\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nma = { path = \"../a\" }\n",
            ),
            ("b/src/lib.rs", "pub fn b() { ma::a() }\n"),
        ],
    );
    let p = provenance_of(&f);
    for want in ["Cargo.toml", "a/Cargo.toml", "b/Cargo.toml", "Cargo.lock"] {
        assert!(
            p.files().contains_key(&RelPath::parse(want).unwrap()),
            "{want} missing from {:?}",
            p.files().keys().collect::<Vec<_>>()
        );
    }
}
