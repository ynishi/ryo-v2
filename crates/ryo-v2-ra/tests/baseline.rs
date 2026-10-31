//! Baseline difference and revert text, on a throwaway workspace (the corpus is never touched).

use ryo_v2_core::app::ports::{Analyzer, AnalyzerError};
use ryo_v2_core::app::verify::verify;
use ryo_v2_core::domain::{Diagnostic, Edit, ErrorCode, FindingCode, RelPath, Scope};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_ra::{RaAnalyzer, RaConfig, RaSession};
use std::path::PathBuf;
use std::sync::Arc;

const LIB: &str = "pub struct S;\n\npub fn a(s: &S) -> u32 {\n    s.missing()\n}\n";

/// A temp workspace (one crate whose `a` has an E0599) that removes itself on drop.
struct Probe {
    dir: PathBuf,
}

impl Probe {
    fn new(test: &str) -> Self {
        // Not CARGO_TARGET_TMPDIR: `session::load` skips every path with a `target` component,
        // so a workspace under target/ would have no indexed files.
        let dir = std::env::temp_dir().join(format!("ryo-v2-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/lib.rs"), LIB).unwrap();
        Self { dir }
    }

    fn load(&self) -> (RaAnalyzer, RaSession) {
        let a = RaAnalyzer::new(
            RaConfig {
                proc_macro: false,
                threads: Some(1),
                assert_revert: true,
                unresolved_paths: true,
                assert_revert_every: None,
                cfg_test: false,
            },
            Arc::new(MonotonicClock::new()),
            Arc::new(RecordingObserver::default()),
        );
        let s = a.load(&self.dir).expect("load probe");
        let e0599 = FindingCode::Rustc(ErrorCode::parse("E0599").unwrap());
        let baseline = a.baseline(&s);
        assert_eq!(baseline.len(), 1, "got {baseline:?}");
        assert_eq!(baseline[0].code, e0599);
        (a, s)
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn lib_edit(extra: &str) -> Edit {
    Edit::write(
        RelPath::parse("src/lib.rs").unwrap(),
        format!("{LIB}{extra}"),
    )
}

#[test]
fn added_copy_of_baseline_error_fails() {
    let p = Probe::new("added_copy");
    let (a, mut s) = p.load();
    let edit = lib_edit("\npub fn b(s: &S) -> u32 {\n    s.missing()\n}\n");
    let v = verify(&a, &mut s, &[edit], Scope::Affected).expect("verify");
    assert!(!v.passed(), "an added copy of a baseline error is new");
    assert_eq!(
        v.new_errors.errors,
        vec![Diagnostic {
            code: ErrorCode::parse("E0599").unwrap(),
            path: RelPath::parse("src/lib.rs").unwrap(),
            line: 8,
        }]
    );
}

#[test]
fn revert_restores_load_time_text_not_disk() {
    let p = Probe::new("revert_text");
    let (a, mut s) = p.load();
    // The disk changes after load; the database still holds the load-time text.
    std::fs::write(p.dir.join("src/lib.rs"), "pub struct S;\n").unwrap();
    let edit = lib_edit("\npub fn c() -> u32 {\n    1\n}\n");
    for round in 0..2 {
        let v: Result<_, AnalyzerError> =
            verify(&a, &mut s, std::slice::from_ref(&edit), Scope::Affected);
        let v = v.unwrap_or_else(|e| panic!("round {round}: {e}"));
        assert!(v.passed(), "round {round}: {:?}", v.new_errors);
    }
}
