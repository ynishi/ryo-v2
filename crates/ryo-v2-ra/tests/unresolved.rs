//! The detector of unresolved type names and path prefixes, on throwaway workspaces.

use ryo_v2_core::app::ports::Analyzer;
use ryo_v2_core::app::verify::verify;
use ryo_v2_core::domain::{Diagnostic, Edit, RelPath, Scope};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_ra::{RaAnalyzer, RaConfig, RaSession};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const BASE: &str = "pub struct S;\n";

/// A temp workspace (one crate) that removes itself on drop.
struct Probe {
    dir: PathBuf,
}

impl Probe {
    fn new(test: &str) -> Self {
        // Not CARGO_TARGET_TMPDIR: `session::load` skips every path with a `target` component.
        let dir =
            std::env::temp_dir().join(format!("ryo-v2-unresolved-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/lib.rs"), BASE).unwrap();
        Self { dir }
    }

    fn load(&self, unresolved_paths: bool) -> (RaAnalyzer, RaSession) {
        let a = RaAnalyzer::new(
            RaConfig {
                proc_macro: false,
                threads: Some(1),
                assert_revert: true,
                unresolved_paths,
                assert_revert_every: None,
                cfg_test: false,
            },
            Arc::new(MonotonicClock::new()),
            Arc::new(RecordingObserver::default()),
        );
        let s = a.load(&self.dir).expect("load probe");
        assert!(a.baseline(&s).is_empty(), "got {:?}", a.baseline(&s));
        (a, s)
    }

    /// New errors of writing `text` to src/lib.rs.
    fn errors_of(&self, unresolved_paths: bool, text: &str) -> Vec<Diagnostic> {
        let (a, mut s) = self.load(unresolved_paths);
        let edit = Edit::write(RelPath::parse("src/lib.rs").unwrap(), text.to_owned());
        verify(&a, &mut s, &[edit], Scope::Affected)
            .expect("verify")
            .new_errors
            .errors
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `(code, line)` of the diagnostics whose code is one of E0405, E0425, E0433, sorted.
fn detector_codes(errors: &[Diagnostic]) -> Vec<(String, u32)> {
    let mut got: Vec<(String, u32)> = errors
        .iter()
        .filter(|d| matches!(d.code.as_str(), "E0405" | "E0425" | "E0433"))
        .map(|d| (d.code.as_str().to_owned(), d.line))
        .collect();
    got.sort();
    got
}

fn pairs(items: &[(&str, u32)]) -> Vec<(String, u32)> {
    items.iter().map(|(c, l)| ((*c).to_owned(), *l)).collect()
}

#[test]
fn reports_unresolved_type_and_prefix() {
    let text = "pub struct S;\n\npub fn f(_x: Missing) -> u32 {\n    0\n}\n\npub fn g(_d: &dyn fmt::Display) -> u32 {\n    0\n}\npub fn h<T: Nope>(_t: T) {}\npub fn k() { let _ = Binn::new(); }\n";
    let p = Probe::new("positive");
    let on = p.errors_of(true, text);
    assert_eq!(
        detector_codes(&on),
        pairs(&[("E0405", 10), ("E0425", 3), ("E0433", 7), ("E0433", 11)]),
        "got {on:?}"
    );
    let off = p.errors_of(false, text);
    assert!(detector_codes(&off).is_empty(), "got {off:?}");
}

#[test]
fn no_findings_on_resolvable_paths() {
    let text = r#"pub struct S;

pub enum E {
    A,
    B,
}

impl E {
    pub fn a() -> Self {
        Self::A
    }
    pub fn b(&self) -> Option<Self> {
        match self {
            Self::A => None,
            Self::B => Some(Self::B),
        }
    }
}

pub trait Tr {
    type Item;
}

pub fn gen<T: Iterator>(t: T) -> Option<T::Item> {
    let mut t = t;
    t.next()
}

pub fn proj<T: Tr>(_x: <T as Tr>::Item) {}

pub fn prims(_a: u32, _b: &str, _c: bool) -> usize {
    str::len("ab")
}

pub fn strs() -> String {
    String::new()
}

pub fn dbg(_x: &dyn std::fmt::Debug) {}

pub mod m {
    pub fn up(_x: crate::S, _y: super::S) {}
}

macro_rules! mk {
    ($n:ident) => {
        pub fn $n(_x: NotHere) -> u32 {
            0
        }
    };
}
mk!(made);

#[cfg(any())]
pub fn gone(_x: Missing) -> u32 {
    0
}

pub fn local() -> u32 {
    struct L;
    fn inner(_x: L) -> u32 {
        1
    }
    inner(L)
}
"#;
    let p = Probe::new("negative");
    let got = p.errors_of(true, text);
    assert!(detector_codes(&got).is_empty(), "got {got:?}");
}

#[test]
fn failed_import_is_not_reported_again() {
    let text = "pub struct S;\n\nuse crate::nothere::Ghost;\n\npub fn g(_x: Ghost) {}\npub fn q() {\n    let _ = Ghost::new();\n}\n";
    let p = Probe::new("failed_import");
    let got = p.errors_of(true, text);
    assert!(
        got.iter().any(|d| d.code.as_str() == "E0432"),
        "ra reports the import: {got:?}"
    );
    assert!(detector_codes(&got).is_empty(), "got {got:?}");
}

#[test]
fn dedupes_with_ra_e0425() {
    let text = "pub struct S;\n\npub fn d(_x: Missing2) -> u32 { missing_value }\n";
    let p = Probe::new("dedupe");
    let got = p.errors_of(true, text);
    let e0425: Vec<_> = got.iter().filter(|d| d.code.as_str() == "E0425").collect();
    assert_eq!(e0425.len(), 1, "got {got:?}");
    assert_eq!(e0425[0].line, 3);
    // Without the detector the one E0425 is ra's own, for the value.
    let off = p.errors_of(false, text);
    let ra_e0425: Vec<_> = off.iter().filter(|d| d.code.as_str() == "E0425").collect();
    assert_eq!(ra_e0425.len(), 1, "got {off:?}");
    assert_eq!(ra_e0425[0].line, 3);
}

/// Findings as `code path:line message`, sorted.
fn baseline_keys(a: &RaAnalyzer, s: &RaSession) -> Vec<String> {
    let mut keys: Vec<String> = a
        .baseline(s)
        .iter()
        .map(|f| format!("{:?} {}:{} {}", f.code, f.path, f.line, f.message))
        .collect();
    keys.sort();
    keys
}

fn load_corpus(path: &Path, unresolved_paths: bool) -> (RaAnalyzer, RaSession) {
    let a = RaAnalyzer::new(
        RaConfig {
            unresolved_paths,
            ..RaConfig::default()
        },
        Arc::new(MonotonicClock::new()),
        Arc::new(RecordingObserver::default()),
    );
    let s = a.load(path).expect("load corpus base");
    (a, s)
}

#[test]
fn corpus_bases_add_no_baseline_findings() {
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/base");
    for name in ["breakage-lab", "extract-trait-multicrate", "macro-lab"] {
        let path = corpus.join(name);
        let (a_on, s_on) = load_corpus(&path, true);
        let on = baseline_keys(&a_on, &s_on);
        drop(s_on);
        let (a_off, s_off) = load_corpus(&path, false);
        let off = baseline_keys(&a_off, &s_off);
        let mut added = on.clone();
        for k in &off {
            if let Some(i) = added.iter().position(|x| x == k) {
                added.remove(i);
            }
        }
        println!(
            "base {name}: on {} off {} added {}",
            on.len(),
            off.len(),
            added.len()
        );
        for k in &added {
            println!("  added: {k}");
        }
        assert!(added.is_empty(), "false positives on {name}: {added:?}");
    }
}

#[test]
fn impl_head_trait_and_self_type_get_their_own_codes() {
    let text =
        "pub struct S;\n\nimpl Absent for S {}\nimpl Ghost2 {}\nimpl Absent2 for Ghost3 {}\n";
    let p = Probe::new("impl_head");
    let got = p.errors_of(true, text);
    assert_eq!(
        detector_codes(&got),
        pairs(&[("E0405", 3), ("E0405", 5), ("E0425", 4), ("E0425", 5)]),
        "got {got:?}"
    );
}

#[test]
fn cfg_disabled_statements_are_skipped() {
    let text = "pub struct S;\n\npub fn c() -> u32 {\n    #[cfg(any())]\n    {\n        let _x: Missing = todo!();\n    }\n    #[cfg(any())]\n    let _y: Missing3 = Binn::new();\n    0\n}\n";
    let p = Probe::new("cfg_stmt");
    let got = p.errors_of(true, text);
    assert!(detector_codes(&got).is_empty(), "got {got:?}");
}
