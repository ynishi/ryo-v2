//! Which file and line a diagnostic is reported against, on throwaway workspaces.

use ryo_v2_core::app::ports::Analyzer;
use ryo_v2_core::domain::{Finding, FindingCode};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_ra::{RaAnalyzer, RaConfig};
use std::path::PathBuf;
use std::sync::Arc;

/// A temp workspace (one crate) that removes itself on drop.
struct Probe {
    dir: PathBuf,
}

impl Probe {
    fn new(test: &str) -> Self {
        // Not CARGO_TARGET_TMPDIR: `session::load` skips every path with a `target` component.
        let dir =
            std::env::temp_dir().join(format!("ryo-v2-attribution-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        Self { dir }
    }

    fn write(&self, rel: &str, text: &str) {
        std::fs::write(self.dir.join(rel), text).unwrap();
    }

    /// The baseline findings of the workspace, as `(code, path, line)`, sorted.
    fn baseline(&self, gap_detectors: bool) -> Vec<(String, String, u32, String)> {
        let a = RaAnalyzer::new(
            RaConfig {
                proc_macro: false,
                threads: Some(1),
                assert_revert: true,
                gap_detectors,
                assert_revert_every: None,
                cfg_test: false,
                engine: false,
            },
            Arc::new(MonotonicClock::new()),
            Arc::new(RecordingObserver::default()),
        );
        let s = a.load(&self.dir).expect("load probe");
        let mut got: Vec<(String, String, u32, String)> = a
            .baseline(&s)
            .iter()
            .map(|f: &Finding| {
                (
                    code_name(&f.code),
                    f.path.to_string(),
                    f.line,
                    f.message.clone(),
                )
            })
            .collect();
        got.sort();
        got
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn code_name(code: &FindingCode) -> String {
    match code {
        FindingCode::Rustc(c) => c.as_str().to_owned(),
        FindingCode::Analyzer(n) => n.clone(),
    }
}

const LIB: &str = "include!(\"part.rs\");\npub fn h(_: MissingType) {}\n";
const PART: &str = "// part\npub fn g() { let _ = missing_value; }\n";

/// `(code, path, line)` of the findings, sorted.
fn keys(got: &[(String, String, u32, String)]) -> Vec<(String, String, u32)> {
    got.iter()
        .map(|(c, p, l, _)| (c.clone(), p.clone(), *l))
        .collect()
}

fn key(code: &str, path: &str, line: u32) -> (String, String, u32) {
    (code.to_owned(), path.to_owned(), line)
}

#[test]
fn included_file_error_is_reported_against_the_included_file() {
    let p = Probe::new("included");
    p.write("src/lib.rs", LIB);
    p.write("src/part.rs", PART);
    let got = p.baseline(false);
    // The one error is in part.rs; lib.rs, which pulls the code in, has none of its own.
    assert_eq!(
        keys(&got),
        vec![key("E0425", "src/part.rs", 2)],
        "got {got:?}"
    );
}

#[test]
fn detector_hit_is_kept_when_ra_reports_the_same_code_and_line_in_another_file() {
    let p = Probe::new("dedup");
    p.write("src/lib.rs", LIB);
    p.write("src/part.rs", PART);
    let got = p.baseline(true);
    // ra's error is in part.rs line 2; the detector's MissingType is in lib.rs line 2.
    assert_eq!(
        keys(&got),
        vec![
            key("E0425", "src/lib.rs", 2),
            key("E0425", "src/part.rs", 2)
        ],
        "got {got:?}"
    );
    assert!(got[0].3.contains("MissingType"), "got {got:?}");
}

/// The premise that keeps an attributed finding inside the Affected scope: rust-analyzer loads
/// an `include!` target only from the including crate's own source root, and the scope always
/// holds whole crates, so the included file is diagnosed and compared wherever the including one
/// is. If this test fails (a target in another crate's directory is loaded), the paths the
/// baseline difference and the revert asserts compare must also take the paths of the findings,
/// or an existing error of the included file is new and the re-assert reports a revert mismatch.
#[test]
fn include_of_another_crates_file_is_not_loaded() {
    let p = Probe::new("cross-crate");
    std::fs::write(
        p.dir.join("Cargo.toml"),
        "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for (name, lib) in [
        ("a", "include!(\"../../b/src/part.rs\");\n"),
        ("b", "pub struct B;\n"),
    ] {
        std::fs::create_dir_all(p.dir.join(name).join("src")).unwrap();
        std::fs::write(
            p.dir.join(name).join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        )
        .unwrap();
        p.write(&format!("{name}/src/lib.rs"), lib);
    }
    p.write("b/src/part.rs", PART);
    let got = p.baseline(false);
    assert_eq!(
        keys(&got),
        vec![key("macro-error", "a/src/lib.rs", 1)],
        "got {got:?}"
    );
    assert!(got[0].3.contains("failed to load file"), "got {got:?}");
}
